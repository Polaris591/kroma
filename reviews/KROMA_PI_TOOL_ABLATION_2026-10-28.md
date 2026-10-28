# Kroma / Pi tool ablation - DeepSeek High

Date: 2026-10-28

## Experiment

Three arms, same WorkBuddy DeepSeek High model and credentials, no production
checkout changes:

- `pi`: native `read`, `bash`, `edit`, `write`, no extensions/MCP/skills.
- `shell`: benchmark Kroma runtime with unchanged built-in Shell only.
- `editwrite`: **same benchmark Kroma runtime**, with working `edit` and
  `write` registered in addition to Shell; original Agent Loop untouched.

All runs used isolated work directories and independent acceptance. Each of
two Python tasks (records implementation, cross-file bug fix) ran twice per
arm. One nontrivial Rust WAL session-ordering regression, injected into
identical Git-archived Kroma source, ran once per arm. Order rotates across
blocks. All 15 trials completed and passed. Kroma source base commit:
`fedfb8595e7352fa2ae0566014f445536486da6c`.

Raw data (private per-run output and full request usage):
`/tmp/kroma-tool-ablation-99ff2ea594/results.json`

## Four small Python runs per arm (two task types x two repeats)

| Metric | Pi | Kroma Shell | Kroma Shell + Edit/Write |
| --- | ---: | ---: | ---: |
| External acceptance | 4/4 | 4/4 | 4/4 |
| Wall seconds | 58.52 | **51.99** | 64.05 |
| Input tokens, cache included | 60,111 | **23,365** | 37,386 |
| Uncached input tokens | 9,295 | **6,853** | 9,226 |
| Output tokens | 2,860 | **2,370** | 3,311 |
| Requests | 24 | **20** | 24 |
| Tool completions | 25 | **19** | 24 |

The additional tools were actually used: Edit/Write arm performed 2 `edit`
and 2 `write` calls across four runs. Compared with Shell-only, the extra tools
raised wall time by about **23.2%**, and total input by about **60.0%** in this
small sample. Do not optimize core for more tools on this evidence.

## One real Rust repair per arm

| Metric | Pi | Kroma Shell | Kroma Shell + Edit/Write |
| --- | ---: | ---: | ---: |
| External WAL regression | PASS | PASS | PASS |
| Wall seconds | **75.794** | 104.273 | 92.381 |
| Input tokens, cache included | **135,737** | 211,449 | 191,405 |
| Uncached input tokens | 16,569 | 27,129 | **15,149** |
| Cached input tokens | 119,168 | 184,320 | 176,256 |
| Output tokens | **4,163** | 4,564 | 4,173 |
| Model requests | **13** | 15 | 17 |
| Tool completions | 20 | **14** | 23 |

Compared with Kroma Shell-only, Kroma Edit/Write cut **11.4% wall time** and
**9.5% total input**, but added **2 model requests** and **9 tool calls**.
All four Edit tools were actually invoked. The provider's cache-read token
share was higher in the Edit/Write arm (92.1% vs Shell's 87.2%), but this
ratio is **not** a request-level cache hit rate or a fee calculation.

Kroma Rust execution profiles:

- Shell: model 63.53 s; total tool time 39.62 s; 15 model requests, 14 Shell calls.
- Edit/Write: model 60.47 s; total tool time 30.92 s; 17 model requests,
  19 Shell calls plus 4 Edit calls. Compiler/test cache warming and differing
  agent plans affect these durations; avoid causal attribution from one run.
- Pi: 13 model requests, 16 Bash calls, 1 Read call and 3 Edit calls.

Classifying tool commands in the Rust traces:

- Shell-only Kroma: roughly 10 inspection calls, 3 cargo test calls, 1 fmt.
- Edit/Write Kroma: roughly 15 inspections, 4 cargo test calls, 4 direct edits.
- Pi: roughly 13 Bash inspections, 1 Read, 3 Edit, 2 cargo test, 1 fmt.

These are categories of **calls**, not independent judgments of code quality.

## Entire five-task sample per arm

| Metric | Pi | Kroma Shell | Kroma Shell + Edit/Write |
| --- | ---: | ---: | ---: |
| Pass | 5/5 | 5/5 | 5/5 |
| Wall seconds | **134.32** | 156.26 | 156.43 |
| Input tokens | **195,848** | 234,814 | 228,791 |
| Uncached input | 25,864 | 33,982 | **24,375** |
| Cached input | 169,984 | 200,832 | 204,416 |
| Cache-read token share | 86.8% | 85.5% | 89.3% |
| Requests reporting nonzero cache reads | **37/37 (100%)** | 30/35 (85.7%) | 36/41 (87.8%) |
| Output tokens | 7,023 | **6,934** | 7,484 |
| Model requests | 37 | **35** | 41 |
| Tool completions | 45 | **33** | 47 |

These totals weight the one long Rust task heavily and should **not** be used
as a universal Harness ranking. Both Kroma variants are nearly tied on total
wall time here. The sampling error is large, particularly for Rust (n=1).
The cache-read-positive request ratio only means the upstream reported *some*
cached input for that request; it is not a cache lookup hit rate or a share of
the full prompt served from cache. Pi may benefit from stable prefixes warmed
by earlier tasks; the test rotated order but did not control provider cache TTL.

## Decision and limitations

Keep Shell as the default. Do **not** merge Edit/Write into the production
core on this evidence. Retain the isolated benchmark harness as a prototype,
and consider user-installable opt-in tools only if longer matched trials
confirm a real benefit. Real caching prices are unknown; Pi's `cacheWrite`
count and Kroma's generic usage do not establish identical price semantics.

Recommended next controlled trial: repeat the Rust regression several times
with a rotating arm order, add a second independent Rust feature task, and
compare `read+bash+edit+write` with `shell+edit+write` only after normalizing
tool descriptions and provider prefix accounting. Track foreground model time,
test execution time, context bytes, requests, real tool usage, independent
acceptance and newly introduced defects. Do not change TRC/APEC policy or
make performance claims from cached-token percentages alone.

## Reproduction

```sh
cargo build --locked --offline --example tool_ablation
python3 benchmarks/tool_ablation_live.py \
  --tasks records,cross_file,rust_regression \
  --python-repeats 2 --rust-repeats 1 --timeout 240
```

The Rust example is benchmark-only. Production `src/` code was not edited and
no Git commit or remote push has been performed for this experimental work.
