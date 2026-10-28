# Kroma P9 batch 2: minimal provider recovery and task-outcome statistics

## Scope

- Base: `f81dc7f`; no new queue, circuit breaker, provider registry,
  permission system, Shell timeout or monitoring service.
- `provider.rs`: reuse existing bounded foreground retry loop. Allow
  connect failures and HTTP 429/502/503/504 as well as timeouts, **only
  before receiving any stream body bytes**. Backoff is 250/500/750 ms
  or a numeric Retry-After seconds value. Never override Retry-After
  above 10 seconds or unparseable HTTP-date forms; return the error.
  401 and other permanent statuses are not retried. Partial streams
  and partial model tool calls are not replayed.
- `agent.rs`: keep existing TaskStats, with one lightweight RAII
  summary guard. Explicit success/errors report `completed`/`failed`;
  dropping the active task future on Ctrl+C reports `cancelled`.
  Reports only successfully completed model requests, with unknown
  usage for failed/in-flight attempts. It makes no extra model calls.

## Verification

- Offline Rust tests: **74 passed / 0 failed / 4 ignored**.
- Strict Clippy / rustfmt / release build: passed.
- `python3 benchmarks/p9_batch2_regression.py`:
  - 429 Retry-After 0 -> 503 -> successful SSE, 3 total requests.
  - 401 authentication error, exactly 1 request.
  - 429 Retry-After 60, exactly 1 request (no early retry).
  - 1 completed Shell tool then HTTP 401, exactly 2 requests,
    failed-task metrics retain 1 completed model request and tool.
  - Midstream partial tool call / truncated SSE: no tool command
    executed, exactly 1 request, durable interruption note.
  - Ctrl+C of a partial response: cancelled-task statistics printed
    and process stops.
- Existing P9 batch 1 normal+legacy overflow and P8 profiling/DeepSeek
  SSE mock regressions all passed.

## Limitations

- Rate-limit Retry-After HTTP-date is not parsed. The conservative
  behavior is to return rather than retry earlier than instructed.
- Network failure after a complete request has reached the provider
  may still incur unknown model generation cost; the code never
  claims failed requests were free.
- Cancelled tasks report known completed work, but APEC requests that
  were still in flight may be unreported. The summary is not a
  precise invoice.
- The batch did **not** include a new real WorkBuddy benchmark; these
  are deterministic integration regressions, not speed evidence.
