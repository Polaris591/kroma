# P7 final report — A/B/F prompt ablation on real coding tasks

Date: 2026-10-26 (Asia/Shanghai). Source: `/root/kroma-p7-abf/results.json`,
per-trial stdout/stderr, isolated source checkouts and independent acceptance.

## Executive decision

**All 72 scheduled trial stages have finished; 68/72 finished normally,
while 72/72 passed the independently executed code acceptance checks.**
Do not equate external code acceptance with complete agent-task success.

- **F** is the fastest overall observed arm, reduces model/tool calls and
  tokens, and beats A on the 15 Rust development stages that all arms
  successfully finished.
- **A** was the most reliable end to end: 24/24 normal completions,
  including all five final Rust integration-review stages.
- **B** performed best on the small Python task suite, but its full Rust
  integration reviews are costlier and it had one abnormal termination.
- **F must not be promoted solely on the aggregate time result:** three
  of its five Rust integration-review stages ended with streaming
  timeouts, versus one for B and zero for A. The fast completion-time
  aggregate is partly a *censored* comparison: a failed run can stop
  before delivering the final user-visible answer.

Recommended disposition: preserve the current frozen experiment,
repair and regression-test streaming failures, then run a smaller
pre-registered P8 reliability/control comparison of A and F (plus B
where Python coverage is required). Prefer F only if it preserves the
observed efficiency advantage **and** achieves acceptable normal
completion. No production prompt was changed for P7.

## Method

- Frozen Rust seed and single built runtime; only the first sentence of
  the projected system execution guidance differed between A, B, F.
- 5 independent Rust development repetitions per arm, each containing
  4 continuing stages: remove, rename, JSON CLI, final integration
  review. 60 Rust stages total.
- 2 repetitions of 2 isolated Python coding tasks per arm. 12 Python
  stages total.
- Arm-order rotation within stages; independent external Rust CLI,
  format, clippy, test and hidden Python regression checks.
- Real WorkBuddy proxy DeepSeek (High) model; **no agent per-task
  deadline/token quota**. The runtime HTTP client still imposes a
  120-second whole-request timeout.
- Exact runtime histories and source artifacts remain in the P7 lab.

The variants are extremely similar; no broad causal assertion about
the effect of a single clause is warranted from this workload alone.
Five repetitions are not independent replications across unrelated
project domains.

## Overall totals — 24 same task stages per arm

| Metric | A | B | F |
| --- | ---: | ---: | ---: |
| Normal agent completion | **24/24** | 23/24 | 21/24 |
| External code checks passing | 24/24 | 24/24 | 24/24 |
| Total elapsed seconds, including failed runs | 3789.62 | 3908.80 | **3136.02** |
| Foreground completed model requests | 337 | 330 | **271** |
| APEC completed model requests (parsed from logs) | 35 | 39 | **33** |
| Combined completed model requests | 372 | 369 | **304** |
| Shell tool calls | 375 | 402 | **317** |
| Foreground uncached input tokens | 938785 | 929604 | **724929** |
| APEC uncached input tokens | 464043 | 446362 | **393836** |
| Combined uncached input tokens | 1402828 | 1375966 | **1118765** |
| Combined cached input tokens | 10190976 | 11159040 | **7145472** |
| Combined output tokens | 436559 | 419655 | **312625** |

Relative F/A observed totals: 17.25% shorter elapsed time,
19.58% fewer foreground model requests, 15.47% fewer shell calls,
20.25% fewer **combined** uncached input tokens. **Do not interpret**
these as net time-to-success improvements until F's three failures
are handled. Provider billing rates and usage of unsuccessful requests
are not known and have not been invented.

## Reliability and failure analysis

Only the `finish` Rust stages failed at the agent level:

| Trial | Exit status | Independent checks | Error |
| --- | --- | --- | --- |
| B / Rust repeat 2 / finish | 1 | All pass | Stream body connection/read timeout |
| F / Rust repeat 3 / finish | 1 | All pass | Stream body operation timeout |
| F / Rust repeat 4 / finish | 1 | All pass | Stream body operation timeout |
| F / Rust repeat 5 / finish | 1 | All pass | Stream body operation timeout |

**Normal final integration-stage completion: A 5/5, B 4/5, F 2/5.**
Three F failures are consistent with hitting the 120-second
whole-request HTTP timeout, including an exact 120-second last
request in the captured profiles. B's failure happened after
approximately 37 seconds of the last request. It may have a different
network cause. These observations do not establish that F creates
provider timeouts: backend load, streamed reasoning behavior, random
variation, and request duration are confounders.

The model-request counter in standard trial summaries counts *only
completed* request usage records. Unfinished requests are not free:
their real token usage is unreported. No APEC errors were recorded.

## Workload split

### Python — 4 small tasks per arm

| Metric | A | B | F |
| --- | ---: | ---: | ---: |
| Normal completions | 4/4 | 4/4 | 4/4 |
| Total seconds | 123.39 | **100.21** | 116.41 |
| Foreground model requests | 29 | **24** | 29 |
| Shell calls | 37 | **26** | 35 |

B is 18.79% faster than A on the small Python sample; sample size
and task diversity are limited.

### Rust development — 15 non-integration stages per arm

Here **all 45 trials finished normally**, avoiding the failure-induced
time comparison problem.

| Metric | A | B | F |
| --- | ---: | ---: | ---: |
| Normal completions | 15/15 | 15/15 | 15/15 |
| Total seconds | 2152.54 | 2001.12 | **1791.10** |
| Foreground requests | 210 | **179** | 184 |
| Shell calls | 225 | 203 | **198** |
| Combined uncached input tokens including APEC | 656493 | 639570 | **550143** |

F is 16.79% faster than A here and finishes 12/15 matched stages
faster than A. B uses slightly fewer model requests than F on
these stages. The per-repeat pattern varies considerably: in Rust
repeat 4, F was *slower* than A.

### Rust final integration review — 5 trials per arm

| Metric | A | B | F |
| --- | ---: | ---: | ---: |
| Normal completions | **5/5** | 4/5 | 2/5 |
| Elapsed seconds including failed runs | 1513.69 | 1807.46 | 1228.51 |
| Foreground requests completed | 98 | 127 | 58 |
| Shell calls | 113 | 173 | 84 |

The F elapsed-time total in this table is **not** a verified
complete-task efficiency win: three runs terminated abnormally.

## Mechanisms and further issues to investigate

1. **P0 — streaming reliability:** `src/provider.rs` sets a whole-request
   120-second Reqwest timeout; long healthy streams may be interrupted.
   Test separate connect timeout and idle-chunk timeout rather than
   simply removing failure detection. Persist a durable interruption
   indicator, and never blindly replay partially executed Shell tools.
2. **P0 — shell exit-code truth:** prior real traces contained three
   `cargo test | tail` / `pytest | tail` false-success statuses. Test
   `pipefail` semantics including SIGPIPE from `head`; do not
   conflate passing the last pipeline process with passing tests.
3. **P1 — final-answer behavior:** measure how often A/B/F call more
   tools after all requested verification is successful. Keep exact
   logs and distinguish meaningful regression checks from redundant
   ones. Current regex `trace` counts are heuristic only.
4. **P1 — APEC economics:** APEC uncached inputs are substantial.
   Separate foreground and APEC billing metrics, and count unsuccessful
   model request attempts separately (usage unknown). Do not reduce
   summary fidelity based on token costs alone.
5. **P2 — wider benchmarks:** add varied projects, languages, long
   sessions, fault injection, and controlled retries after fixing
   transport; the P7 Rust task family is deliberately narrow.

## Next action

Do not re-run the finished P7 lab, retroactively edit its results,
or overwrite the production prompt. Implement the minimal transport
and Shell fixes **after** freezing this report, validate them with
deterministic offline tests, then use the *same* stabilized runtime
for a focused A/F evaluation of normal completion and efficiency.

Project code and credentials were not modified as part of this report.
