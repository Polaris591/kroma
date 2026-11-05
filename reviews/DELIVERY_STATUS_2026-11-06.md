# Kroma audit and official plugins: evidence-based delivery status

Date: 2026-11-06. Base: `main` at `985ebb1`; development branch:
`feat/audit-fixes-official-plugins-20261106`.

This report distinguishes shipped functionality, new patches, local tests and
acceptance work that **cannot yet be marked complete**. No paid model or web
search API calls, installation changes, or user-session rewrites were made.

## Previously completed work (present on main)

- Prior audit fixes: snapshot locking/cancellation and recovery; plugin content
  approval, tool hooks, lifecycle and isolated sessions; cache budget
  calibration; TUI/desktop regressions. See `BATCH_1_2_FIXES_2026-11-05.md`,
  `BATCH_3_4_PERFORMANCE_ARCHITECTURE_2026-11-05.md` and
  `COMPACTION_CACHE_DIAGNOSIS_2026-11-05.md` for actual scope and evidence.
- PR #1 (`985ebb1`): stale signed Anthropic thinking is excluded from both
  normal and checkpoint projections; failed request cache trace accepts
  provider-reported usage. Neither is a real Anthropic provider acceptance test.
- Web plugin 1.6.0: `web_search` across configured paid APIs and keyless
  fallbacks, `web_fetch` with bounded continuation, URL filtering, HTTP or
  Firecrawl extraction, error classification and tests.
- Project Memory R0/R1: SQLite project scoping; quote-verified user rules;
  7/30-day pending/review expiry; task-scoped automatic recall and summary;
  explicit forget/off controls; no daemon or cloud dependency.
- Subagents 0.3.0: isolated Kroma child sessions; queue with bounded
  concurrency; ordered results; progress, evidence, resume and cancellation.

## Newly developed / validated in this branch

- Subagent 1.0 profiles: builtin/user/project Markdown profiles and precedence,
  `profile`/`label`/`complexity` tool arguments, configured model tiers,
  independently enforced tool filtering, restricted shell mode and per-task
  turn/token admission limits. Child execution uses Kroma itself; no second
  Agent Loop was added. Defaults inherit model/thinking rather than silently
  selecting DeepSeek. Restricted shell policies are not OS sandboxes.
- Project-memory auto-recall does not inject unrelated most-recent entries,
  while still allowing bounded matching on distinctive words in a larger
  query. Durable history and rejected/forgotten entries are unchanged.
- Corrected a cancellable Git checkpoint test's missing Tokio runtime and
  cleaned up seven Rust Clippy findings without suppressing lint rules.
- New focused tests cover profile parsing/scope/validation, unsupported
  task input, read-only shell dispatch and project-memory recall relevance.

## Test evidence on Linux Devspace

- Core Rust lib: **642 passed, 9 ignored** with the additional restricted
  shell dispatch test; targeted subagent policy: **3 passed**.
- Core `cargo clippy --locked --lib -- -D warnings`: passed before the final
  additional dispatch test; all Rust changes are formatted.
- Project-memory Rust tests: **43 passed**.
- Node web+subagent final tests: **151 passed**, including **65** subagent
  tests after profile expansion.
- Plugin host `plugins_e2e`: **52 passed**, 0 failed when rerun with two test
  threads. Initial high-concurrency run was 51/52 and a streaming model worker
  exited without a full response; isolated rerun and full two-thread rerun
  passed. The intermittent failure's cause was not identified.
- Live *keyless* search smoke: two general queries returned results via Bing,
  but first results were broad/generic, not sufficient to demonstrate high
  relevance. Live HTTP fetch and extraction of official Rust OnceLock docs
  succeeded. This is not the required 20-query comparative evaluation.

## Open items — do not treat these as fixed

1. **Protocol/cache:** actual outbound post-hook/after-serialization cache
   evidence, provider request IDs and a verified root cause for Responses
   zero-hit periods remain unavailable. Anthropic preserved-thinking behavior
   under mid-tool-loop compaction still needs endpoint/model-mode acceptance.
2. **Compression strategy:** TRC+APEC experiments are **not** validation for
   the present lazy single-pass strategy. Controlled same-model/end-point
   crossover tests and foreground checkpoint timeout/cost measures are open.
3. **Web:** 20-query authoritative-source relevance/latency/cost evaluation
   per provider and credentialed API acceptance remain open; do not spend
   paid provider quota without the account owner's explicit approval.
4. **Project memory:** R2's full review ledger, conflict isolation, periodic
   limit/merge policy, two-week real-use quality targets and R3 embedding/team
   features are not complete. R3 is explicitly deferred in the design.
5. **Subagents:** live provider execution with profile tier routing, hard
   process-tree teardown in every OS, full plugin-to-real-child acceptance,
   and profile-specific behavior under a Windows/macOS agent are not verified.
6. **Cross-platform + release:** full integration-suite CI, Windows/macOS
   interactive UI acceptance, and release artifact distribution were not
   performed in this Devspace session. Root filesystem has been near full;
   keep builds bounded and avoid counting build failures as product failures.

These gaps are not a release-blocker assertion for functionality users are not
using, but they prevent any claim that *every* latest review issue and all
future-phase plugin acceptance criteria are resolved.
