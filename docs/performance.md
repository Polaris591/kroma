# Opt-in local performance measurements

These measurements answer where local work is spent. They do not call a paid
provider, assert universal latency targets or change runtime execution policy.
Run them in an optimized build with one test thread, without unrelated builds
or benchmark processes competing for the machine:

```sh
cargo test --release --locked --lib perf_ -- --ignored --nocapture --test-threads=1
cargo test --release --locked --lib bench_streaming_frame_latency_three_hundred_and_three_thousand_messages -- --ignored --nocapture --test-threads=1
cargo test --release --locked --lib bench_large_session_save -- --ignored --nocapture --test-threads=1
```

`bench_large_session_save` (`src/session.rs`) is a single-sample session I/O
lane, not a `PERF {json}` fixture: it builds a 65-message history of about 4 MiB,
then times 12 full-journal rewrites against 12 WAL appends and prints one
`session-io` line with bytes, `full_rewrite_ms` and `wal_append_ms`.

The ordinary test suite skips these timing fixtures. Run it separately:

```sh
cargo test --locked --tests
cargo clippy --locked --all-targets -- -D warnings
```

## Visual artifact lanes

Rendering goldens and screenshots are separate ignored tests; they call no model
and need no credentials, and they write files instead of asserting latency:

```sh
cargo test --locked --lib -- --ignored --nocapture export_plain_preview
cargo test --locked --lib -- --ignored --nocapture calm_transcript_golden_matrix
cargo test --locked --lib -- --ignored --nocapture export_graphite_visual_gallery
KROMA_SNAPSHOT_DIR=target/snapshots cargo test --locked --lib redesign_snapshot_matrix
```

- `export_plain_preview` (`src/tui/plain_tests.rs`) renders the real `UiState`
  through `TestBackend` and writes HTML/ANSI previews under
  `reviews/tui-plain-runtime/`.
- `calm_transcript_golden_matrix` (`src/tui/redesign_snapshots.rs`) compares 25
  scenes across 3 presets and 4 sizes with the committed text goldens in
  `reviews/tui-graphite/final/`; set `KROMA_UPDATE_SNAPSHOTS=1` to rewrite them
  (they still encode the pre-card layout, hence the ignore).
- `export_graphite_visual_gallery` (`src/tui/redesign_snapshots.rs`) writes the
  Graphite visual gallery.
- `redesign_snapshot_matrix` (`src/tui/view_tests.rs`) always asserts the layout
  and writes text frames only when `KROMA_SNAPSHOT_DIR` names an output
  directory.

## Scenarios and interpretation

| Scenario | What is timed | What is not established |
| --- | --- | --- |
| File checkpoints | Root discovery and the cancellable private-store transaction; 1,000 files of 1 KiB, first capture, unchanged and one-file edits. | The store is preinitialized; first capture has one sample, not a cold-start latency distribution. No giant repository or network filesystem claim. |
| Summary source planning | Repeated prefix shrinking on 201 messages / about 133 KiB JSON with an 1,800-token input cap; old/current planners alternate in one process. | This is the constrained-input path, not the typical no-compaction request or a provider token count. |
| Checkpoint prepare | Full built-in prepare with an immediate or 30 ms delayed fake summary and a recording commit. | Mock delay/usage are not real provider latency, retries, disk WAL cost or billable API usage. |
| Automatic title | Synchronous scheduling separately from completion of a 30 ms fake request and catalog update. | A local scheduling measurement is not foreground end-to-end latency; OS timers can exceed the requested delay. |
| Tools | Direct fake tool, registry and no-op interceptor, plus real `echo perf` process startup/capture. | Schema/dispatch nanoseconds do not predict remote MCP/plugin/tool workloads. No tool concurrency is enabled. |
| TUI | TestBackend draws, warm streaming windows, forced full-history rebuilds, expanded output and code blocks. | No GPU, actual terminal I/O, process RSS or universal frame-time guarantee. Forced rebuilds are intentionally different from the normal cached path. |
| Session I/O | 12 full-journal rewrites versus 12 WAL appends on a 65-message history of about 4 MiB (`bench_large_session_save`). | One process, one local filesystem and one sample per path; no fsync or disk-latency distribution, no realistic transcript mix and no crash-recovery cost. |

`PERF {json}` lines report scenario, fixture, sample count, p50/p95/min/max in
nanoseconds, process/platform and debug-build status. Timed regions exclude the
reporting itself. TUI's existing report uses microseconds. A sample for fake-tool
dispatch averages 1,000 calls; checkpoint and title samples time one operation.
Samples are small and timer/system-load sensitive; p95 may be the largest sample.
Do not use these numbers as flaky CI pass/fail thresholds.

## Controlled planner experiment

The retained planner optimization builds and media-projects
the checkpoint source once, then truncates that same view as candidate boundaries
shrink. Previously every candidate rebuilt/projected the complete source again.
Token estimation, safe tool boundaries, summary output reserve and provider
reasoning provenance remain unchanged. A frozen test-only reference planner
checks exact selected boundary, serialized source and token estimate; existing
context/protocol tests cover the surrounding request/commit contracts.

The alternating old/current measurements isolate this change more reliably than
comparing separate runs under different system load. See the
[batch report](../reviews/BATCH_3_4_PERFORMANCE_ARCHITECTURE_2026-11-05.md)
for actual local results and raw logs. A win on this fixture is not an
end-to-end task speedup claim.

## Decisions deliberately deferred

Snapshots are currently delayed until the first tool call (but not until the
first write). This avoids snapshots for text-only answers, not for read-only
tool tasks. The data does not by itself justify delaying them further,
changing compression retries/thresholds, disabling title generation or
executing tools concurrently. Keep those policies unless a separate
representative, correctness-preserving experiment supports a change.

For a real task, `KROMA_PROFILE=1` records phase timing including `snapshot_ms` and
request/tool counts. Profiling JSON byte counts adds serialization work, so leave
it off normally. Provider-reported usage can still be unknown; rough token/JSON
estimates are not an API invoice. No live-provider benchmark is run implicitly.

## Frame timing

Set `KROMA_PROFILE_FRAMES=1` to log any draw longer than 16 ms and any event-loop gap over 250 ms. A gap means the loop itself was blocked (for example by a synchronous compaction step), while a slow draw means rendering and wrapping cost the time. Both are opt-in because they write to the runtime log.
