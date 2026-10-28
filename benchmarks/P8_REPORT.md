# Kroma P8 delivery and validation

Date: 2026-10-26 (Asia/Shanghai)

## Baseline

Before P8, the exact P7-tested working source plus evidence was committed
as **`af03225`** (`chore: freeze P7-tested Kroma baseline and benchmark
evidence`). The project was not reset, and the user’s previous uncommitted
work was preserved in that baseline.

## P8-A — F deployment and reliability repairs

- Production `src/agent.rs` uses the exact **F** clause from P7:
  `For coding tasks, focus inspections on relevant files; once the needed evidence is found, edit rather than re-inspecting unchanged files.`
  The dependent-call ordering and required-test instructions remain intact.
  There is no per-task step/time/token budget, automatic prompt selection,
  new approvals, or other architectural expansion.
- Replaced the global 120-second Reqwest HTTP deadline with a
  **20-second connect timeout** and an independent **300-second
  stream idle timeout**, reset on each received network chunk. Healthy
  streams may run for more than 120 seconds in aggregate. Only timeouts
  *before* any delivered stream data are retried, up to three times.
  Completed Shell calls are never replayed.
- Non-stream APEC model calls retain a separate 120-second deadline
  with existing bounded timeout retries. Old TRC/APEC semantics are
  otherwise unchanged.
- A model stream that fails after partial visible output now records
  a durable interruption note in the session WAL. A partial assistant
  answer or partial tool call is never treated as complete. Existing
  durable Shell results remain in place for an explicit follow-up.
- Bash Shell runs with `pipefail` so `cargo test 2>&1 | tail` returns
  the failing upstream test status. Because `head` can terminate an
  upstream producer via SIGPIPE, the tool description explicitly
  explains exit 141 instead of misidentifying it as a failed test.

## P8-B — targeted efficiency and cost accounting

- The selected F guidance reduces evidence-free re-inspection without
  enforcing arbitrary call limits.
- Logs show each attempted foreground model request, including
  attempts that do not produce a usable Usage block. Errors report
  their token cost as **unknown**, not as zero.
- Task completion logs now include combined foreground+APEC
  effective uncached/cached input usage when reported by the
  provider, separately identifying foreground and APEC request
  counts. Combined usage is `n/a` when any necessary provider
  measurement is unavailable.
- **Decision:** No speculative changes to APEC summary chunking,
  hard thresholds, or context reconstruction. P7 proved significant
  APEC cost but did **not** establish that reducing chunk count would
  preserve correctness. That optimization requires a targeted
  factual-recall benchmark; avoiding an unvalidated alteration
  is intentional, not an unfinished code path.

## P8-C — verification

- Offline Rust tests cover healthy sustained streaming longer than
  a configured idle interval, mid-stream incomplete output timeout,
  first-byte timeout retry, persistent interruption after failed
  streaming, pipefail with failed and successful pipelines, and
  SIGPIPE exit 141 from `head`. Existing session, TRC, APEC,
  tool-output, and shell-cancellation regression suites also run.
- `cargo fmt --check`, `cargo clippy --locked --offline --all-targets -- -D warnings`,
  `cargo test --locked --offline`, and
  `cargo build --release --locked --offline` were run without
  modifying provider credentials.
- An additional live, isolated Python third-party repository coding
  regression was executed via `benchmarks/p8_live_regression.py`.
  Its independent acceptance runs **after** the agent finishes and
  verifies the frozen hidden test, upstream regression tests, and
  saved WAL session. Results are stored at
  `/root/kroma-p8-regression/result.json`.
  **Observed result:** exit 0, 29.638 elapsed seconds, 8 completed
  model requests, 8 Shell calls, hidden acceptance pass, upstream
  tests pass, hidden test unchanged, and durable session present.

## Remaining limitations / next empirically justified work

- A healthy *real-world* 120+ second model stream has not yet been
  observed during this P8 live coding test; the timeout behavior
  was instead tested with accelerated local SSE mocks. Real
  WorkBuddy stability may vary.
- A streaming response that has partially arrived is intentionally
  **not** automatically replayed. It leaves an interruption marker
  and requires an explicit user follow-up to resume safely.
- Shell `pipefail` catches test pipelines but cannot magically
  detect failures hidden behind an explicit `; echo success`,
  `|| true`, or other deliberate command masking.
- P7's 72-stage workload is narrower than arbitrary repositories
  and projects. One live P8 Python task plus offline fault tests
  is a regression check, not a universal reliability guarantee.
- APEC factual-retention and billing optimization should be
  investigated only with a separate measured benchmark; no
  new APEC algorithm has been added.
