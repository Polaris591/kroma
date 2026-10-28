# Kroma TRC/APEC four-way experiment — delegated task

You are Pi Coding Agent running on the Kroma server, model **cobblemon/gpt-6.1-sol**, thinking **high**. Execute this task to completion in the current invocation. Do not merely write a plan.

## Goal
Run reproducible **real WorkBuddy DeepSeek** tests comparing:
- A: APEC only (`KROMA_FUSION_EXPERIMENT=a`)
- B: APEC + basic TRC (b)
- C: APEC + cache-aware TRC (c)
- D: APEC + cache-aware TRC + evidence-aware summarization (d)

All four modes already exist in `src/compression.rs`. The minimal draft `benchmarks/fusion_ablation.py` is currently incomplete (it ends at `usage()`). Finish it, validate fixtures, then run real experiments. Preserve the normal Kroma default behavior.

## Requirements
1. **First inspect** `src/compression.rs`, `src/model.rs`, `src/agent.rs`, `benchmarks/fusion_ablation.py` and local Pi CLI. Make sure the four variants actually have meaningfully different effects under the chosen history. Fix *benchmark harness* defects; do not refactor production code unless the experiment would otherwise be invalid, and explain any production fix.
2. WorkBuddy DeepSeek model is configured in Pi as `workbuddy/deepseek`, endpoint previously used `https://cobblemon.asia/wb/v1`. Check current configuration without printing credentials. Use an authorized connection/API configuration only. **Never reveal, log, commit, echo or write any keys or tokens**. If you cannot connect safely, report this as a blocked experiment; do not fabricate usage.
3. Set up isolated sandbox under `/tmp/kroma-fusion-ablation`: same initial history, same coding task, same model and tools; reset project and session for every run. Check a real code fix (median even cases), exact tests unchanged, and historical QA code `KROMA-QA-27` recalled. Use an additional distinct fixture if the only task is too easy to reveal memory loss.
4. First run a **smoke run per mode**. Confirm TRC actually triggers in B and/or C under proper context pressure, and APEC calls occur in all modes. If the same strategy silently runs in all modes, **mark that test invalid** and adjust the fixture/budget. A is strictly APEC-only; D must differ from C in the summary input. For caching experiments, keep path/prompt stable and use repeat ordering to reduce warm-cache bias.
5. If smoke tests work, do **two independent runs per variant** (8 total) within practical budget, timeouts 90-150 seconds each and capped retries. Do not run open-ended or overnight. Do not allow a model to make changes outside the sandbox. Record input/output/cached tokens separately for foreground and all APEC calls, latency, tool count, TRC cleared count, APEC summary calls, correct code fix, final test pass, constraint/QA recall, errors/timeouts. If precise usage is unavailable mark `n/a`, not zero. Explain sampling uncertainty. If run failures are API/env problems, classify separately from algorithm failures.
6. Implement robust cleanup and outputs `/tmp/kroma-fusion-ablation/results.csv`, `summary.md`, and per-run sanitized logs. No secrets in generated artifacts. If token usage instrumentation has gaps, note them and avoid a false cost comparison.
7. Validate with `cargo fmt -- --check`, `cargo test --locked`, `cargo clippy --locked --all-targets -- -D warnings` and `python3 -m py_compile benchmarks/fusion_ablation.py`. Avoid excessive testing.
8. Report in Chinese: confirmed completed runs, results table, substantive differences, unexpected findings, statistical limits, and recommended next engineering step. Do not choose a winner based only on compressed token count or cache hit ratio.

Avoid installing new dependencies, running destructive commands or touching unrelated services. There is no initial Git commit in this repository. You have permission to edit the benchmark script and run the sanctioned Kroma sandbox experiment in this session. Execute now.
