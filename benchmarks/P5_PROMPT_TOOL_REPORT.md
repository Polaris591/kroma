# P5 — prompt and tool-description factorial study (2026-10-26)

Production checkout `/root/kroma-new` was **not** modified for this study.
Experiment script: `benchmarks/p5_prompt_tool_factorial.py`; isolated runtime
and detailed machine-readable outputs: `/tmp/kroma-p5-prompt-tool/`.

## Hypotheses and design

- A: Current P4 baseline system prompt + Bash tool description.
- B: Change only the first coding-guidance sentence to recommend focused
  searches and editing after enough evidence, rather than re-inspection.
- C: Change only the tool description to a concise Bash interface contract;
  leave system guidance untouched.
- D: B + C together (interaction test).

Same existing WorkBuddy DeepSeek High model/proxy, unchanged executable
logic, compression, task text, fixture setup, Python upstream tests and
hidden acceptance checks. Each arm handled two independent boltons tasks
(IPv6 regression fix; digit masking feature) twice, with fresh checkouts;
execution order was counterbalanced. No whole-agent time/token limit.
All four arms passed tool-use calibration, each with two model requests
and one tool call.

| Arm | Correct | Mean seconds | Model requests | Tool calls | Noncached input | Cached input |
|---|---:|---:|---:|---:|---:|---:|
| A | 4/4 | 33.10 | 32 | 36 | 23,734 | 139,392 |
| B | 4/4 | **21.56** | **24** | **31** | 30,947 | **86,400** |
| C | 4/4 | 32.81 | 36 | 44 | 24,155 | 145,664 |
| D | 4/4 | 26.44 | 29 | 36 | 24,408 | 126,464 |

B vs A: 34.9% lower mean wall time, 25.0% fewer completed model
requests, 13.9% fewer tools, 38.0% fewer cached input tokens, but **30.4%
more uncached input tokens**. This is *not* a verified billing-cost win.
All variants passed every acceptance test and the upstream test suite.

The concise tool-description change C did **not** help on its own; it
increased model/tool calls despite similar average wall time. Combined D
was less effective than B. Thus changes are not simply additive.

## Limits and next validation gate

The four paired tasks cover only two small Python coding problems, two
repetitions each. Network/provider and model stochasticity may still
affect measured performance. The only experimental change in B is a
single system-guidance sentence; that should be retested on an independent
multi-stage Rust development workload before production adoption.

Do **not** change the production tool description or claim the optimal
global prompt is proven. Retain P4 as the production baseline until a
Rust longitudinal test establishes correctness, ordinary termination,
tool efficiency and uncached/cached token behavior. A/B acceptance and
cost are to be evaluated together rather than speed alone.

The results are observational with a small experimental sample; do not
attribute a universal causal gain or extrapolate to Pi without a new
head-to-head run.
