# Kroma: audit and official plugins delivery ledger (2026-11-06)

This is a **verification ledger**, not a claim that the entire four-module
audit or three official-plugin roadmaps have been completed. Only items marked
`verified` have local evidence; conditional provider risks must not be called
confirmed production failures without protocol-specific reproduction.

## Verified changes in this development worktree

| Area | Fix or acceptance evidence | State |
| --- | --- | --- |
| Context capacity | Effective output reserve never falls below the model's resolved wire output limit; regression added | verified unit test |
| Shell timeout | Preserve already captured stdout/stderr and return an explicit timeout marker in a failed result | verified unit test |
| Shell live preview | Buffer split UTF-8 code points across preview ticks; no duplicate replacement characters for valid text | verified unit test |
| Cancelled checkpoint | Synchronous Unix Git checkpoint no longer spawns Tokio child without reactor; cancellation cleans its process group and releases lock | verified unit test |
| Web provider costs | Paid auto failover and model-selected backend changes require explicit user settings, with regression tests | verified Node tests |
| Project memory recall | No recency-only injection of unrelated memories; fallback finds distinctive overlapping search terms | verified Rust tests |
| Project memory maintenance | At most 200 active entries; archive old review/reference notes before explicit rules | verified Rust tests |
| Project memory in children | Subagent worker permits reads but blocks memory mutations and task-summary auto capture | pure guard test; full child-host integration not run |
| Subagent defaults | No hardcoded DeepSeek/maximum reasoning settings; invalid task options rejected | Node tests for task validation |
| Nested subagents | Hide the unavailable `subagent` tool from a Kroma child | core integration not run |
| Responses streaming | Assemble all ordered final message items instead of dropping later items after text deltas; two regressions | verified Rust tests |
| Subagent jobs | Keep single Kroma session per task; support `run/status/wait/continue/cancel` and durable status after completion | verified Node tests; host integration not run |
| Subagent background protocol | Stop sending stale progress deltas after returning a background job handle | verified Node test |

## Still open: recent core-module audits

1. **Agent Loop:** reconcile confirmed correctness issues against current code,
   including completion-length handling and restart-safe turn identities;
   separately benchmark snapshots before changing default task behavior.
2. **Model protocol/provider:** extended mock and live tests for Responses
   refusals, Anthropic signed-thinking tool cycles (extended vs
   adaptive), and all failed/retried request-usage accounting.
3. **Context/cache:** end-to-end valid first/second compactions, actual outbound
   sanitized fingerprints and provider request-ID correlation, long synchronous
   checkpoint waits, and same-model/endpoint ablation of lazy single-pass vs
   older TRC+APEC. The Responses zero-cache incident remains **unexplained**;
   do not attribute it to Anthropic behavior.
4. **Tool runtime:** benchmark Bash startup and verify timeout/UTF-8 semantics
   across Windows/macOS/Linux. No cross-platform verification performed here.

The historical audit in `COMPRESSION_CACHE_DEEP_AUDIT_2026-11-05.md` also lists
conditional compatibility risks and optimization opportunities; these require
evidence, not automatic unbounded changes. Prior batches `BATCH_1_2_FIXES` and
`BATCH_3_4_PERFORMANCE_ARCHITECTURE` document earlier fixes and must not be
recounted as new work.

## Still open: official plugin completion criteria

- **Web** (`examples/plugins/web`): search/read plugin is usable and its
  simulated network tests pass. A no-extra-Node-runtime packaged distribution
  and full native PDF extraction are not implemented. The 20-query *live* cost/relevance study in
  `examples/plugins/web/evaluation.md` is not complete. API credentials,
  account permission for billed calls, and authoritative source relevance
  measurements are needed. No live-provider reliability claim is made.
- **Project memory**: R0/R1 implemented. R2 is **partial**: automated active
  cap, recall relevance and child read-only behavior added; conflict quarantine,
  one-time dispute prompts, file fingerprint staleness, and structured review
  run ledger are not fully implemented or verified. R3 embedding/team sharing
  is explicitly a conditional *non-default* research direction.
- **Subagents**: the existing process executor now exposes the later,
  simpler 2.0 job actions `run/status/wait/continue/cancel` while retaining
  old batch inputs; the two default roles are general/explore, both
  instruction-only (not a sandbox). Live TUI model/profile switching is not
  yet passed into child connections, so the default child model may differ
  from the selected parent model; a named saved provider profile can be
  configured instead. Native host integration, distribution, and durable
  cancellation after a worker restart remain unverified/unavailable. Do not
  claim 2.0 has been fully accepted end-to-end.

## Verification and environment

- Node web + subagent suite: 157 passed, 0 failed.
- Project-memory Rust suite: 45 passed, 0 failed.
- Core context suite: 44 passed, 2 pre-existing performance tests ignored.
- Focused Responses, output reserve and shell timeout/UTF-8 tests: passed.
- Full core Rust library suite: **644 passed, 0 failed, 9 ignored** with
  `NO_COLOR` unset, `TERM=xterm-256color` and `RUST_TEST_THREADS=1`. Initial
  failures were mostly because Devspace exports `NO_COLOR=1` and `TERM=dumb`;
  the remaining checkpoint reactor panic was a real bug and was fixed. Native
  Windows/macOS runs and all integration suites are still unavailable.
- Entire Rust integration suite, desktop, npm distribution and all native OS
  targets **not verified**. Devspace root disk (39 GiB) is almost full, so
  running all targets risks a linker failure unrelated to code.
- No production provider calls or paid search-quality comparisons performed.

Before merging further architecture changes, test the exact current commit,
check code against its acceptance criterion, and do not hide environment
failures behind a green result for an unrelated narrower test.
