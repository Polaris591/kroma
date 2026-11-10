# Round 14 — Real GitHub historical PR bug ablation (2026-11-10)

## Verdict

**Evidence-based finishing guidance remains the leading experimental candidate, but do NOT merge or enable it by default yet.** The new real-source PR trial supports improved normal completion and some savings, but a meaningful portion of the planned 8–12 open-source issues is still untested, all four runnable cases are JavaScript projects (no genuine Rust PR scored), and the test environments were reduced to the authentic core source plus targeted Node test harnesses rather than full upstream dependency+CI environments.

## Provenance: historical, not synthetic bug insertion

Exact pinned pre-fix upstream `index.js` and `package.json` were fetched from GitHub at the PR **base commit SHA**, stored in `benchmarks/round14_fixture_*.json` with file blob SHAs. The p-limit trials also pinned the actual `yocto-queue@1.2.1` source and `package.json`, vendored into the throwaway task fixture so npm installation was unnecessary. No code flaw was manually injected; the defect is present in the historical upstream code. Local public tests and later-injected hidden checks are **benchmark-specific adaptations** using Node's built-in test runner, not claims that the full original AVA/XO/TSD suites passed.

| Real open-source PR | Historical base commit | Issue | Expected behavior |
|---|---|---|---|
| https://github.com/sindresorhus/p-map/pull/88 | `65aaa8f4d7e757a5254a146c4c39403efa9e2139` | pMapIterable index tied to async completion order | index tied to input order |
| https://github.com/sindresorhus/p-map/pull/91 | `3ada5f36632aca8df860c376856270b6d2ba2de8` | early-aborted signal retains listener | no listener leak |
| https://github.com/sindresorhus/p-limit/pull/108 | `42599ebbbb1228a5bdab381fcf8f4ac20eb8d551` | detached limit.map fails | detached call retains concurrency |
| https://github.com/sindresorhus/p-limit/pull/109 | `df476048d023ff868cd45b35ee47f5fb0ca2b25a` | limitFunction does not expose clearQueue | queued work rejected correctly with rejectOnClear |

Negative controls: all four pre-fix source snapshots passed the supplied public tests and **failed their independent hidden behavioral tests**. Thus the task is not satisfied just by rerunning an existing green suite.

## Valid trials only

Same existing Kroma research release binary, DeepSeek v4.1 Flash High and endpoint, same tools/result formatting/context controls, one prompt selector varying between baseline and `KROMA_EXPERIMENT_PROMPT_ARM=finish`. Two independent repetitions per PR, order reversed on second repetition. Each agent must repair the source, run targeted tests itself, receive positive independent oracle **after** exit, and finish normally under cap. Every trial has its own project directory and WAL.

| PR | Repetition | Baseline seconds | Evidence-finish seconds | Baseline strict completion | Candidate strict completion |
|---|---:|---:|---:|---|---|
| p-map #88 | 1 | 82.906 | **48.656** | yes | yes |
| p-map #88 | 2 | ≥120.109 (timed out) | **74.829** | no (code accepted) | yes |
| p-map #91 | 1 | 58.125 | **24.016** | yes | yes |
| p-map #91 | 2 | **65.375** | 91.813 | yes | yes |
| p-limit #108 | 1 | **26.078** | 32.188 | yes | yes |
| p-limit #108 | 2 | 47.782 | **32.532** | yes | yes |
| p-limit #109 | 1 | **29.218** | 56.125 | yes | yes |
| p-limit #109 | 2 | 60.265 | **32.640** | yes | yes |

**Summary of 16 VALID runs:** baseline 7/8 strict completion, candidate 8/8. All 16 final sources passed isolated behavioral oracle and agents ran required tests. Per-pair time: candidate 5 wins, baseline 3 wins. Sum of measured/capped wall time baseline **489.858s**, candidate **392.799s** (~19.8% less), but the baseline includes a 120-second censored observation; do **not** interpret this as a universal 19.8% acceleration. Tool calls across all runs: 116 vs 96. Among only the **seven pairs both arms normally finished**, baseline 369.75 seconds, 73 model requests, 551,315 input tokens, 95 tools; candidate 317.97 seconds, 58 requests, 368,601 input tokens, 77 tools. These are descriptive totals of seven pairs, not significance estimates.

## Independent audit and invalid test handling

`benchmarks/round14_real_pr_recheck.py` reran all 16 post-agent hidden behavioral tests, verifying public tests were unchanged and upstream base source refs were present: **16/16 results confirmed**, no mismatches.

An **earlier invalid p-limit #109 trial** was discarded: its hidden test attached the expected Promise rejection handler after `clearQueue()`; Node reported `unhandledRejection` even on correctly fixed implementations. The test was repaired to attach `assert.rejects` *before* clearing, and the entire p-limit batch was re-run in a clean `kroma-round14-real-plimit-v2` lab. Do not count or pool invalid `kroma-round14-real-plimit/results.json` rows in any ranking.

Previous Round 12 Unicode session-preview bug fixture also had a **missing system message**, making the original archive unreadable. It was repaired; the new isolated negative-control test run in `kroma-round14-unicode-corrected` now fails with the intended assertion, `snippet omitted actual match: "…尾尾尾..."`. The old Round 12 A/B acceptance results remain **invalid**. We did **not** rerun that historical A/B yet, so it cannot be used to support candidate correctness or incorrectness.

Research-worktree Kroma built-in tests were checked on isolated `D:/Kroma/target-kroma-round13-qa`: **783 passed, 0 failed, 11 ignored**. Formatting and `git diff --check` passed; prior Clippy was clean. No production defaults, main branch, or one-shot threshold compaction were changed.

## Scope and limitations

- **Four** real upstream merged PRs, not the proposed **8–12** issues. This is a completed first batch, not a final go/no-go benchmark of diverse authentic projects.
- The upstream repositories could not be fully cloned due intermittent GitHub HTTPS failures from the Windows machine. GitHub API file fetches reliably supplied exact historical core source files. Their full test infrastructure and transitive dev dependencies were intentionally **not** installed; Node built-in tests were substituted. Their code is genuine upstream, but this is not a full checkout/CI replication.
- No real Rust historical-PR task passed candidate selection/preflight yet. A Rust Unicode-width PR was inspected but the generated tables and codegen process make a faithful Windows fixture more involved. Do not secretly replace with a synthetic Rust task.
- The final time caps are model/network/weather-dependent; one baseline cap created a censored observation. Model stochasticity also produces pair inversions, e.g. p-map #91 repeat 2 and p-limit #109 repeat 1.
- Source code correctness and task normal completion are separate: a code-correct timeout is still strict incomplete.
- The candidate's observed evidence-based stopping benefit may be task-dependent and is not proof of statistical significance given 4 bug families and 2 repetitions.

## Data and scripts

- `D:/Kroma/experiments/kroma-round14-real-prs/results.json` — p-map valid runs.
- `D:/Kroma/experiments/kroma-round14-real-plimit-v2/results.json` — p-limit valid runs.
- `D:/Kroma/experiments/kroma-round14-real-prs/combined_recheck.json` — independent 16/16 post-run recheck.
- `benchmarks/round14_real_pr_cases.py` and `benchmarks/round14_real_plimit_cases.py` — fixtures and A/B runners.
- `benchmarks/round14_real_pr_recheck.py` — validation rerun.
- `benchmarks/round14_unicode_recheck.py` — repaired Unicode negative control.
- `benchmarks/round14_real_issue_discovery.py` — upstream discovery, documents unsuccessful full clones.

## Recommendation

**Keep the baseline as the production prompt; evidence-finish is the leading candidate.** Proceed with more truly independent open-source issues, especially a real Rust regression with independently reproducible tests and separate Cargo build caches, before a default merge. No planner, verifier, task-classifier, API approval, forced stop, or complex Prompt-policy dispatch is justified. The research branch retains experiment switches disabled by default.
