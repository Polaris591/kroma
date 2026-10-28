# Kroma P9 Batch 1: overflow recovery and durable failure handling

## Baseline and scope

- Base Git: `d75d1ea` (P8); changes confined to `shell.rs`,
  `context.rs`, `session.rs`, `agent.rs`, tests and documentation.
- Existing architecture unchanged: one Shell tool, prompt F, cache-aware
  TRC + raw-history APEC, sequential tool execution, JSON snapshot + WAL.
- No new LLM calls, approvals, command timeouts, quota systems or memory
  algorithm added.

## 1. Large-output failure fix

- Shell now preserves output streams larger than **64 KiB** in original
  capture files; the agent sees head/tail preview, original file path,
  byte count and an explicit warning that unseen evidence may matter.
  This is an *inline preview threshold*, not a limit on Shell output.
- For legacy inline outputs or a batch of medium-sized results that exceed
  the hard context threshold, the outgoing request can preview the
  largest **unconsumed** tool messages. Their raw session messages are
  unchanged and remain in the durable checkpoint/WAL. This emergency
  projection does not mark any unread result as TRC-consumed.
- Normal short tool output and conservative duplicate-line filtering
  are unchanged. Excessively large non-tool prompt/history still
  correctly fails rather than silently deleting user content.

## 2. WAL failure handling

- The session journal cursor is marked unavailable after append,
  sync, checkpoint, or snapshot I/O failure. Subsequent attempts
  to append/save or begin another Agent run in that process are rejected,
  requiring restart/reopen to perform validated WAL recovery.
- A test-only partial-write injection creates an incomplete journal
  tail. On restart, the incomplete bytes are discarded; previously
  durable tool results are preserved, and missing results become
  **unknown** rather than re-executed.
- Snapshot-write failure was also induced with an invalid backing
  directory; later mutations are blocked. No database or rollback
  mechanism added.

## 3. Tests and verification

- `cargo test --locked --offline -q`: **74 passed, 0 failed, 4 ignored**.
- `cargo clippy --locked --offline --all-targets -- -D warnings`: passed.
- `cargo fmt`, `git diff --check`, and release build: passed.
- `python3 benchmarks/p9_batch1_regression.py`: offline end-to-end
  mock provider actually runs the updated Release Kroma and a real Shell
  command yielding ~500 KB; provider receives file-backed preview, returns
  the final answer, and restarting the session makes exactly one new
  model request **without re-executing** the prior large Shell command.
- `python3 benchmarks/p9_batch1_legacy_regression.py`: additional offline
  end-to-end recovery of a **pre-P9 saved snapshot** with a 500,000-character
  inline tool result, plus six accumulated 72,000-character results. The
  real Kroma client resumed both sessions without a deadlock. In the six-result
  fixture only 2 of 6 results needed request-only shortening, and the
  complete original checkpoint was verified unchanged.
- A live, isolated third-party Python bug-fix regression was also rerun
  after the Batch 1 release build (separate from the offline mocks).
  Independent hidden acceptance and upstream tests passed; 24.869 seconds,
  7 completed model requests, 7 Shell calls, 0 APEC requests. Evidence:
  `/root/kroma-p9-batch1-real-regression/result.json`.
- `python3 benchmarks/mock_profile_smoke.py`: existing DeepSeek
  reasoning-content replay, Shell tool, streaming and WAL integration pass.
- New Rust tests: 60 KB unchanged versus 100 KB fully captured file,
  unread 500 KB legacy result after saved-history reopen, multiple
  medium-sized unread results, injected partial WAL failure/recovery,
  snapshot I/O failure, 90 KB failed stderr preserved with its nonzero
  exit status, and cancelling an in-flight APEC worker without changing
  original history or persisted compression state.

## Important limitations

- Raw outputs above 64 KiB now require extra reads when the model needs
  omitted intermediate lines. This was intentional to prevent a confirmed
  deadlock; real WorkBuddy latency/accuracy was **not re-benchmarked**.
- Emergency previews are based on an approximate context-token estimator;
  a provider may use a different tokenizer. Hard threshold checks remain.
- Restart/reopen is the conservative recovery procedure after uncertain
  WAL writes; no automatic replay or automatic in-process repair is
  attempted.
- A single live WorkBuddy coding regression passed, but this is not a
  statistically meaningful A/B latency or accuracy comparison. P7 remains
  the prompt-policy baseline; changing the Shell inline threshold may
  trade additional file reads for fewer context tokens on some workloads.
