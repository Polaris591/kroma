# P10: targeted simplification after overdefense review

## Scope and evidence

- Production baseline: `ddc7ef0`.
- No changes to Shell command authority, subprocess lifecycle, WAL,
  model streaming, cache-aware TRC rules, APEC chunk sizes, or APEC retries.

### Saved-profile loading

When `KROMA_MODEL` and `KROMA_BASE_URL` are both explicitly set and
`KROMA_PROVIDER_PROFILE` is not set, Kroma no longer loads unrelated
saved profiles. This avoids blocking fully specified, environment-only
workloads because of malformed, stale, or world-readable saved files;
it also prevents accidentally inheriting a saved API key or thinking mode.
When the environment is incomplete, or a profile is requested by name,
normal stored-profile validation still applies. Saved files are not
rewritten or permissions weakened.

### P9 test isolation

Default and `--ignored` offline test invocations now skip P9 external
model calls unless `KROMA_P9_APEC_LIVE=1` is explicitly set. The optional
historical chunk-count test skips machines without the frozen P7 data.
Online A/B remains separately invokable with the documented opt-in.

### First-use output filtering

The dedicated exact-duplicate-line build-log filter and its command
classifier have been removed. Frozen P7 Rust histories contained 845
Shell results (34 passed the filter's size/command/exit preconditions)
but no eligible repeating-line runs. Additional Python development and
regression WAL histories yielded 113 results and no run of four or more
identical consecutive nonempty lines. These are *observational samples*,
not proof that duplicate logs never occur in npm/pytest/other stacks.
Production remains able to pass through small tool output in full, keep
large results in referenced files, and invoke TRC/APEC as before.

## Risk and reversibility

- A future workload that emits many identical successful build lines
  may send more input tokens without the prefilter. Keep tracking provider
  usage and only revisit if observed savings justify the classifier cost.
- The first-use raw output is still available from the persisted session.
  It was never modified by the removed model-only projection.
- With fully specified environment variables, stored profile selection
  is intentionally ignored unless the caller asks for one.

## Verification

Run:

```sh
cargo fmt --check
cargo test --locked --offline -q
cargo test --locked --offline -- --ignored --nocapture
cargo clippy --locked --offline --all-targets -- -D warnings
cargo build --release --locked --offline -q
python3 benchmarks/p10_config_regression.py
python3 benchmarks/mock_profile_smoke.py
python3 benchmarks/p9_batch1_regression.py
python3 benchmarks/p9_batch1_legacy_regression.py
python3 benchmarks/p9_batch2_regression.py
```

This phase is a complexity reduction, not a claimed model-latency speedup.
