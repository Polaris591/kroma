# BF-13 — Run-tree request evidence and Tournament cost estimates

Status: **SOURCE_IMPLEMENTED / NODE_TESTED / NATIVE_RUST_VALIDATION_REQUIRED** (2026-10-18). This is not a provider invoice or a measured comparative experiment.

## Purpose and non-goals

BF-11 recorded a parent run's completed requests but intentionally left all child and Tournament spend unpriced. BF-13 threads *credential-free* per-request evidence from a real child `AgentRunner` to the parent event log and Kroma's `BenchArmResult`. The independent Foundation evaluator recomputes all measured request prices using the frozen operator price table.

This does **not** change Provider behavior, grant broader sandbox permissions, automatically dispatch paid experiments, establish scorer isolation, or prove a Harness strategy wins. BF-10's provider dispatch safety gate remains closed.

## End-to-end accounting pipeline

1. **Child executor** (`subagents.rs::ProviderSubagentBackend`): collect the `ModelRequestCompleted` events *in the child's own EventLogger*. Retain returned model, wire-request digest, per-request input/output tokens and cache reads/writes; carry the actual transmitted-attempt count separately. Zero attempts can be complete only if the child's recorded summary balances; a retry whose usage is absent, missing model/digest or degraded logging forces `request_usage_complete=false`.
2. **Normal subagent** (`agent_loop.rs`): emit a `ChildRequestUsageRecorded` event for each completed/failed child *before* its `SubagentCompleted` event. Timeout, cancellation, join errors and unsupported delegation have incomplete evidence by default, not a measured zero.
3. **Tournament** (`subagents.rs`, `agent_loop.rs`): carry the same fields for **every** `TournamentScore`, not only the winner. Parent events identify a candidate with a tournament spawn-ordinal prefix so a second race doesn't reuse `tournament-c1` as the first race did.
4. **Native Bench** (`bench.rs`): build `child_request_usage` alongside parent `request_usage`. `children_ledger_complete` requires a 1:1 match for spawned, completed and itemized children, a complete measured record for every transmitted attempt, and an exact reconciliation to the parent children input/cache/output totals. A missing or duplicated child event, dropped event, unmeasured request, or overflow blocks the **entire run** from getting a price estimate.
5. **Independent auditor** (`scripts/bench-foundation.js`): recompute every parent + child attempt against the frozen model-specific table, with distinct rates for uncached input, cache reads, cache creation and output. Reject inconsistent summary arithmetic and duplicate candidate identities. Legacy Tournament results without BF-13 evidence remain **unknown**, including those that claim zero child tokens.

A billed cost is an **operator-supplied price estimate (nano-USD), never an invoice**. If even one attempt cannot be accounted for, `estimated_cost_nano_usd=null`; it must not be shown as $0. Reporting a better pass rate or lower cost based on synthetic tasks is prohibited.

## Shape (abbreviated, no prompts, no raw tool traces)

```json
{
  "child_usage_complete": true,
  "child_request_usage": [{
    "subagent_id": "tournament-3-c1",
    "request_attempts_used": 1,
    "request_usage_complete": true,
    "input_tokens": 200,
    "cached_input_tokens": 0,
    "output_tokens": 50,
    "requests": [{
      "turn": 0,
      "request_identity_sha256": "<sha256-of-wire-request>",
      "reported_model": "provider-returned-model",
      "usage_basis": "provider_reported",
      "input_tokens": 200,
      "cached_input_tokens": 0,
      "cache_creation_input_tokens": 0,
      "output_tokens": 50,
      "estimated_cost_nano_usd": 12345,
      "cost_unknown_reason": null
    }]
  }]
}
```

The exact `reported_model` is required: we do not backfill from the requested model. `request_identity_sha256` is **not** an authorization grant or unique attempt ID. Separate candidates may submit identical requests; scopes prevent accidental cross-child deduplication. Do not publish raw Provider JSON, prompts, tool outputs, environment variables or API credentials.

## How to verify

Local, keyless tests:

```bash
node --test scripts/test-bench-foundation.js scripts/test-bench-phase2.js
node --test scripts/test-kroma-js-upgrade.js scripts/test-release-trust-root.js \
  scripts/test-provenance.js scripts/test-release-pipeline.js \
  scripts/test-release-attestation.js
```

Rust toolchain 1.85.1 (native environment only; no API keys):

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test -p kroma-core bf13_ledger -- --nocapture
cargo test -p kroma-core --test bench_contract --locked
cargo test --workspace --locked
```

### Known limitations / remaining risk

- **Rust compilation is not verified in the authoring container.** The current environment lacks `cargo`, `rustc`, and `rustfmt`; the new Rust fields, matches and tests require independent native verification. Treat results as *source implementation plus Node contract validation*, not a green Rust gate.
- A child attempt that is **transmitted but cancelled before returning a usage response** has unknown cost; the reserved tokens and aggregate success counters cannot recover a provider invoice. Cost stays unknown on purpose.
- The new request evidence is authenticated neither by a provider invoice nor by an external scorer. The isolated evaluator, freeze lock identity, signed verdict and benchmark process boundaries remain separate concerns.
- The sandbox configuration of the existing Provider Bench is not proof of full model-tool isolation; automatic paid Provider dispatch remains disabled pending dedicated isolation validation.
- Full on-device Windows/Android, production Release/attestation and real-model cross-Harness experiments were not run in this BF-13 batch.
- The model window may shift reported usage conventions. A `None` cache split is unknown even if a provider's zero/default semantics might make it safe to price; prefer a conservative null to an undercount.

## Acceptance criteria

BF-13 source is ready for external native review only when Rust checks above pass. It is ready for **real comparative pricing claims only** after trusted Provider+tool isolation, frozen task/price/model identity, actual request logs (including failed child attempts), and independent evidence validation all succeed. None of those conditions may be inferred from a Node test suite.
