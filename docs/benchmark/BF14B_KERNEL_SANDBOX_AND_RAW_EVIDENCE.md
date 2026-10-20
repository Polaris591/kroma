# BF-14B/1: Whole-process sandbox preflight and raw scorer-evidence binding

**Delivery stage:** offline kernel sandbox *supervisor implementation*, defensive tests, and strengthened signed-evidence reviewer. **NOT** a fully authorized real-Provider evaluator; **NOT** an independent scorer; **NOT** a Sandbox certification or successful paid-model benchmark.

## 1. What is implemented

### Trusted Linux parent supervisor

`node scripts/bench-sandbox-supervisor.js probe --out /operator-private/probe-output`

- The executable is exactly `/usr/bin/bwrap`, a root-owned executable that is not group/world writable. **No custom wrapper and no fallback to a host process** is accepted; if bwrap is missing or namespaces cannot be established, exit nonzero with `BF14B_ENV_BLOCKED` and create no successful receipt.
- A private trusted parent creates a writable temporary workspace and a separate, read-only fixed Python probe; the probe is never sourced from a task repository or model output.
- The guest is launched with `--die-with-parent`, `--new-session`, `--unshare-all`, dropped capabilities, `--clearenv`, no Provider credentials, a read-only host *toolchain allowlist*, private `/tmp`, only one host-writable `/workspace`, and no host root/HOME/API credentials mount. The launcher uses `spawnSync(argv)` with `shell:false` and a bounded wall clock and stdout.
- The measured probe checks distinct mount, PID, user, and network namespaces, absence of a default route and host canary, hidden `/etc/shadow`, inability to write outside `/workspace` or to the trusted fixed code, absence of Oracle and scorer mounts, a successful workspace-only write, and **a nested subprocess** that inherits the confinement. Every assertion must pass; otherwise **no PASS receipt**.
- All evidence is stored in the parent-only output directory, not in the agent workspace. A `sandbox-probe.json` and a `supervisor-receipt.json` bind the bwrap/program SHA-256, policy, argv, and the exact checks. No model text/keys/host secret bytes are written to receipts.
- `probe` is a *preflight* run: it does **not** prove that a later arbitrary job used the same OS sandbox. It is **not** BF-14A's job-level signed attestation.

### Actual whole-process synthetic execution

With a previously frozen **synthetic** Foundation lock and a previously compiled native Kroma executable:

```bash
node scripts/bench-sandbox-supervisor.js synthetic \
  --lock /operator/trusted/frozen/lock.json \
  --kroma /operator/trusted/kroma \
  --out /operator/trusted/bf14b-synthetic-results \
  --timeout-ms 120000 \
  --expect-lock-sha256 ACTUAL_64_CHAR_SHA256
```

- Revalidates the frozen Manifest/Suite/price hashes and pinned lock digest before starting anything.
- Copies the exact executable and immutable inputs into parent-private temporary files; mounts the executable at `/runner/kroma` and synthetic Suite at `/frozen/suite.json` **read-only**, the writable workspace at `/workspace`. The entire Kroma CLI process and any descendants are inside the OS process sandbox, not just its final `verify_command`.
- Re-runs the fixed probe **with the same runner and `/frozen` mounts** used by execution, verifying a read-only input mount; then launches the frozen synthetic Rust Benchmark with a bounded wall clock. Uses the existing `bench-foundation.js` `evaluate()` to re-validate the report; does not introduce a second Agent Loop or scoring implementation.
- Saves a redacted report, real preflight probe, and a separate receipt (`synthetic_os_sandbox_plumbing_only`) with source/lock/policy/report digests. The full model/tool raw report stays in a parent-private scratch area and is deleted on exit.
- A frozen `executor: provider` input is **always rejected**, whether bwrap exists or not. This does not make additional paid calls possible.

Example prerequisites:

```bash
command -v bwrap
cargo build --release --locked -p kroma-cli
npm run test:bench
```

### Raw scorer artifact possession and integrity

A signed BF-14A isolation job matrix previously covered **digest strings only**, allowing `eligible_for_manual_review` even if the referenced raw logs were unavailable. It no longer does.

For each `job.run_id` in the signed matrix, supply these four real, operator-protected files:

```text
/operator/separate-evidence/
  sandbox-run-0/
    sandbox-probe.json
    agent-execution.json
    scorer-execution.json
    workspace-snapshot.json
  sandbox-run-1/
    ...same four exact filenames...
```

The evaluator requires `--evidence-root` in addition to `--isolation-proof` and the existing scorer signature/public key/locked report:

```bash
node scripts/bench-foundation.js evaluate \
  --lock /trusted/lock.json --report /trusted/native-report.json \
  --attestation /scorer/scorer-attestation.json \
  --isolation-proof /scorer/isolation-evidence.json \
  --evidence-root /scorer/separate-evidence \
  --scorer-key /trusted/scorer-public.pem \
  --expect-lock-sha256 ACTUAL_64_CHAR_SHA256 \
  --out /private/review-out
```

The verifier requires exact job directory coverage, exactly four files per job, no symlinked file/job/root, each file <=1 MiB, total <=128 MiB, and byte-for-byte SHA-256 equality with the signed job-level digest. It does **not** copy raw logs to `metrics.json` or to the redacted output. Without `--evidence-root`, the signed matrix may still authenticate but **cannot elevate the report to manual promotion review**. Even with the raw bytes, the only permissible status is *manual review*, **never automatic release/promotion**.

**Critical caveat:** these additional bytes can still be fabricated by a malicious or mistaken scorer. Cryptographic consistency ≠ causal kernel proof. A real independent verifier must inspect the unforgeable external supervisor audit trail and test credential denial and actual independent grades before treating results as trustworthy. The supplied Node test fixtures intentionally use fictitious files and test-only Ed25519 private keys.

## 2. Why real Providers remain blocked

This change **does not** authorize Provider dispatch. Current Rust `kroma bench --executor provider` still places task Suite data (including held-out Oracle conditions) in the process address space and exposes Provider authorization through a runtime that can spawn model-requested tools. Placing that whole process in a bwrap sandbox cannot simultaneously hide the loaded Oracle from malicious model-generated tool commands. Unsharing the network also prevents genuine Provider calls. Therefore the following must be delivered **before** enabling paid model execution:

1. A genuine credential-isolated Provider request broker (outside the guest, no keys in model-accessible env/mounts and only allowlisted egress, with per-request costs/budget enforced by the broker).
2. A task input contract that sends only prompt + initial project files into the sandbox, never frozen held-out Oracle/solution/verifier assets. An independent scorer reads these only after the Agent has stopped.
3. Real per-Job kernel probe and resource enforcement (token, dollars, timeout, process/memory, cancellation, retries, cleanup) with separate host-side logs and signed evidence tied to each job; an outer cgroup is required for bwrap RAM/CPU limits.
4. An independently managed signer/scorer with immutably pinned provenance and real oracle results, not merely copied Rust `resolved` fields. Operator must authorize real-model spend and max dollars separately.

The existing legacy manual Rust Provider path has **not** been modified in this slice. It must not be described as a verified safe Benchmark. This preflight/synthetic runner cannot be used to create a real Provider benchmark ranking.

## 3. Security test / acceptance checklist

```bash
npm run test:bench
node scripts/release-gate.js actions
node scripts/release-gate.js dag
node scripts/bench-sandbox-supervisor.js probe --out /unique/probe-destination
```

The first three can execute offline without bwrap. The fourth **must use an actual `/usr/bin/bwrap` binary** and working kernel namespaces. A missing bwrap, blocked namespace, unsuccessful probe, timed-out child or malformed stdout is `BF14B_ENV_BLOCKED`, **not PASS**.

The BF14B test suite executes an unsandboxed copy of the same fixed probe and asserts that it does **not** pass: it detects its own mount/network/user namespace identity and can read the host canary. This is a control demonstrating the checks are not hardcoded to true. When bwrap is present it runs the real preflight, but if the host kernel refuses required isolation the test records a capability block rather than falsely blessing the sandbox.

**No work mode, no network/provider billing, no production key operations, no source push or release were used for this code slice.**
