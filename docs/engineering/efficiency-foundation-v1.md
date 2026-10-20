# Efficiency foundation v1 — candidate, not a performance claim

## Scope and source identity

This development tree imports the repaired delivery archive with SHA256
`8a26308b23eec06bbe952af920c7c8b41c1398906da6af19000316f389d1b014`.
It does not replace the frozen competition source/binary or change historical scores.
The competition-only unbounded-budget changes are deliberately absent.

This increment establishes measurement and one opt-in prompt hypothesis. It does not
claim that Kroma is a next-generation efficiency winner, that all fundamentals are
finished, or that the candidate saves tokens or money.

## What changed

1. `RequestCompositionObserved` is emitted by the actual AgentRunner immediately
   before the provider call, after tool activation and ephemeral context assembly.
   It reports separate payload byte counts for system, logical tool definitions,
   user messages, assistant text/reasoning/signatures/opaque fields, tool arguments,
   tool results and base64 image data. These are UTF-8 bytes, **not tokenizer output**.
2. Per-run memory tracks hashes of the preceding logical request. Events report only
   whether system/tools are unchanged and the length of the unchanged history prefix.
   They contain no content, argument strings, paths or per-message hashes. Usage and
   host-only tool metadata do not affect this comparison. The observer does not edit
   request inputs, add model messages, remove reasoning or change authorization.
3. `--prompt-profile default|efficient-candidate` is explicit on the CLI; `default`
   preserves the original prompt. The host-selected profile is wired into headless,
   bench and RPC session construction (REPL/ACP/serve share that session path). The
   candidate makes code_graph conditional and describes grouping independent calls,
   while retaining fingerprints, minimal edits, relevant testing and honest failure
   reporting. Production default is **not promoted**.

## Observation limitations

* Logical prefix equality is not a measured provider cache hit. The protocol adapter
  may represent reasoning/images differently, and a broker/gateway may transform
  the wire request. No event claims exact wire sizes or cache savings.
* Counts exclude role/ID/name/framing overhead. Tool schemas use internal logical JSON,
  not protocol wire wrappers. Empty schemas still serialize as `[]`.
* The first request (including a fresh observer after resume) reports prefix status
  as unknown. This increment does not persist observer state in a session WAL.
* The observer scans/hashes the projected logical input. It has overhead; the synthetic
  debug-build probe measured approximately 13ms for 256KiB and 80ms for 1.5MiB on this
  host. These are not release measurements, a latency gate, or an inference benchmark.
* The existing `ModelRequestStarted.request_body_bytes` is a historical estimate. This
  increment does not silently relabel it as exact wire bytes. Use the new event's
  explicitly named categories for logical composition only.
* Child agents/repair races may use role-specific builders. This increment validates
  the parent run/bench/RPC prompt seam, not whole-tree prompt inheritance across every
  child/tournament/recovery mode.

## Local acceptance

The new tests cover UTF-8 payload partitioning, content-free events, unchanged
append-only prefixes, tool-order normalization, tool/system/content rewrites, image
and reasoning changes, ignoring host metadata/usage, first-request unknown status,
original default preservation, real AgentRunner pre-dispatch emission, unmodified
model context, real RPC and bench candidate injection, and CLI opt-in parsing.

Use Rust 1.85.1 and the pinned Cargo.lock. For a small disk:

    export CARGO_TARGET_DIR=/path/to/isolated-build
    export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
    cargo fmt --all -- --check
    cargo clippy --offline --workspace --all-targets -j 2 -- -D warnings
    cargo test --offline -p kroma-core --lib -j 2
    cargo test --offline -p kroma-cli --bin kroma -j 2
    cargo test --offline -p kroma-core --test request_composition --test request_composition_runner
    bash scripts/run-workspace-tests.sh
    bash scripts/run-sandbox-tests.sh

The two workspace/sandbox scripts compile many executables at once. On this host the
first whole-workspace compile exhausted disk; no passing verdict was inferred. The
subsequent acceptance compiles and executes all 101 integration-test targets in small
batches, then deletes only this increment's test executables. It does not omit failing
or sandbox targets. Cargo's `profile.test` identifies executables; examples are never
mistaken for test harnesses. Individual logs and JSON results are retained outside the
source repository. Full core and CLI unit suites are separate.

## Next gates, in order

1. Reproduce request-level composition on fresh runs: explain additional calls using
   evidence, distinguish new necessary input from old replay, and separately inspect
   actual wire-prefix stability. Do not infer a cache defect from hit ratio alone.
2. Freeze dev and untouched holdout tasks, arm definitions, model/effort and repetition
   count before paid execution. Compare original baseline, observation-only/default,
   efficient-candidate, and pi. All cells start from clean code and sessions; randomize
   ordering, preserve isolation and frozen external scorers. Original tasks are debug
   material, not untouched holdout. Do not choose the best restart as the sole score.
3. Record completion, all retries/failures, total input/output tokens, cache reads/writes,
   unknown usage, actual declared rate-table cost, wall time and observer overhead.
   Missing rates/usage remain unknown. Token ratio is not invoice ratio.
4. Promote only with non-degraded completion and repeatable benefit on untouched tasks;
   predeclare a non-inferiority margin and uncertainty method before executing. A small
   pilot is hypothesis screening, not proof. Roll back or keep opt-in if inconclusive.
5. Only then address the next proven bottleneck (tool response size, file rereads,
   repeated verification, prefix churn, restoration or process supervision). One factor
   at a time; no automatic context dropping, removed protocol reasoning, weakened tests,
   hidden limits or default tournament escalation to manufacture an improvement.

No paid API requests, production deployment, remote push or release signing are part
of this increment. Local Rust regression success is not cross-platform acceptance.
