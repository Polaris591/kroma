# BF-14A — Signed isolation and independent scoring evidence matrix

**State: implemented as an offline verifier and regression suite; not an OS sandbox and NOT permission to run a real Provider benchmark.**

## Motivation and trust boundary

The existing BF-12 Ed25519 scorer attestation authenticated a statement containing `isolation_proof_sha256` but did **not read the corresponding proof bytes**. A signature over boolean flags (`sandbox_passed`, `all_jobs_isolated`) is only a statement by a key holder; it is not direct evidence of end-to-end confinement. BF-14A closes the missing evidence-file binding and requires an exhaustive job-level accounting matrix.

`kroma bench --executor provider` has a legacy manually launched execution path which must **not** be described as a trusted, fully OS-isolated Benchmark: placing its scratch directory outside the operator's repository and sandboxing the *final verifier* are not sufficient to sandbox every Agent tool. The Foundation `run` command **continues to reject** Provider execution; do not bypass it with a manually synthesized `PASS` flag. BF-14B must implement and prove whole-run isolation before any automatic paid execution.

## BF-14A verification protocol

Freeze a Provider Manifest with the independent scorer **public** key (`freeze --scorer-key ...`) and obtain `lock.json` and the frozen inputs. An independent scorer in a separate administrative/security domain (not the agent being evaluated) must:

1. Execute every `(task_id, strategy, repetition)` under a real OS isolation boundary (bwrap/container) that includes **all tool subprocesses and subagents**, not just the final oracle.
2. Produce task-local sandbox probes and independent oracle logs, verify credential inaccessibility, write confinement, network policy, time/token budget and cancellation enforcement. Record hashes without copying prompts, secrets or raw tool output into the published certificate.
3. Independently decide `oracle_resolved` and `oracle_gate_status` for each job; both must reconcile with the Native Rust report. Reject mismatch; **never copy the Rust result blindly**.
4. Write a UTF-8 JSON evidence file with schema `kroma.benchmark.isolation-evidence/1`, bound to the exact frozen lock and raw Rust report bytes. The scorer's pre-registered Ed25519 key signs its **existing** `kroma.scorer-attestation/1` canonical payload including the SHA-256 digest of this evidence file (`isolation_proof_sha256`).
5. The separate analyst runs `evaluate` with `--isolation-proof` in addition to the existing `--attestation`, `--scorer-key`, and `--expect-lock-sha256`.

Example (signing/actual sandboxing are **not** implemented by this script):

```bash
node scripts/bench-foundation.js evaluate \
  --lock /trusted/freeze/lock.json \
  --report /separate-scorer/native-report.json \
  --attestation /separate-scorer/scorer-attestation.json \
  --isolation-proof /separate-scorer/isolation-evidence.json \
  --evidence-root /separate-scorer/raw-per-job-artifacts \
  --scorer-key /trusted/scorer-public.pem \
  --expect-lock-sha256 64_HEX_FROM_FREEZE \
  --out /read-only-review-archive
```

The evidence root requires the exact schema, frozen experiment/lock/report/suite/source/oracle identities, `sandbox_declaration`, scorer binary SHA-256, scorer environment SHA-256, sandbox policy SHA-256, and `jobs`. Each job must include its unique `run_id`, `(task_id, strategy, repetition)`, `sandbox_backend` (`bwrap` or `container`), digests for the sandbox probe, agent execution log, scorer execution log and workspace snapshot, independently determined `oracle_resolved`/`oracle_gate_status`, and **all eight true** isolation assertions:

- `tool_execution_isolated`, `network_policy_enforced`, `host_secrets_inaccessible`, `workspace_writes_confined`
- `timeout_and_cancellation_enforced`, `budget_enforced`, `oracle_unwritable_by_agent`, `scorer_unwritable_by_agent`

The verifier refuses missing/extra/duplicate jobs, duplicate run IDs, invalid/missing digests, extra free-form fields including secret-bearing `prompt`, OS backend `host-process`, failed flags, independent grade mismatch, unbound evidence, untrusted signatures and symlinked evidence files. **BF-14B additionally requires `--evidence-root` with each job's four actual raw artifact files, all matching signed SHA-256, before the result may even enter manual review**. Raw bytes are not exported in the public summary. See `BF14B_KERNEL_SANDBOX_AND_RAW_EVIDENCE.md`.

**Important caveat:** The new verifier checks that these are **consistent, signed claims backed by digest references**, not that the referenced logs exist, that probes were honest, or that the kernel enforced the requested policy. Evidence digests cannot substitute for external audit of actual bwrap/container commands, mount/user/pid/network namespaces, network-egress controls, credible separate scorer keys and raw denial tests. `eligible_for_manual_review` is *not* promotion, certification, production release or an independently measured Harness improvement.

## Downgrade prevention

Legacy BF-12 scorer signatures **without** a checked per-job isolation evidence file still authenticate as signatures but **cannot** change `needs_isolation_attestation` into `eligible_for_manual_review`. This is a deliberate fail-closed tightening: the old `isolation_proof_sha256` may have been an arbitrary 64-hex string. A complete signed BF-14A matrix without the actual raw artifacts is now insufficient for manual review. Both the existing frozen statistical/safety gates **and** BF-14B raw artifact digests must pass.

## Verification and rollout

```bash
npm run test:bench
node scripts/release-gate.js actions
node scripts/release-gate.js dag
```

Node regression tests are also registered with the CI and Release Node quality steps. They generate **temporary test-only Ed25519 keys and synthetic Provider-shaped JSON**. No Provider APIs, private production keys, paid calls, real OS sandbox or real scorer services are used. Rust sources, Cargo.lock, task Runner, Agent Loop, and OS sandbox implementation are unchanged by this slice.

## BF-14B / subsequent engineering, not complete

1. Build a **real operator-controlled parent process** that creates and probes a separate kernel sandbox for the entire Agent execution tree, including all model-invoked tools, children and Tournament branches. Do not allow fallback to an unconfined host.
2. Protect credentials from model-invoked tool processes even if those run in a filesystem/network sandbox; provide Provider-only network egress via a separately governed proxy or equivalent separation, and test exfiltration attempts.
3. Enforce live wall-clock, token/dollar and process/resource limits with independent termination and test OOM/network/fork bombs, timeouts, retries and cleanup.
4. Build a separate trusted Oracle runner which opens immutable held-out inputs only *after* the agent has stopped, collects actually verified probe logs, and signs the complete job matrix only after inspecting those logs.
5. Cross-validate real-source, real-scorer and cross-Harness experiment reports with controlled, independently held-out tasks. No real paid trials until all prior gates pass and the operator explicitly authorizes spending.
