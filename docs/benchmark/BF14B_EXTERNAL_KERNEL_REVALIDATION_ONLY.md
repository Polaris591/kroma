# BF-14B External Kernel Revalidation Only

**Scope:** run the completed offline supervisor on an authorized Linux host with a real `/usr/bin/bwrap`, then inspect and return authentic logs. **No code development, API keys, paid Provider calls, repository push, secret sharing, production signatures or Release actions.**

## Input

Use only the BF-14B/1 source tree delivered in `Kroma_BF14B1_Source_20261018.tar.gz` or the identical tree after applying `Kroma_BF14B1_20261018.patch` to the BF-14A source. Do not mix other source versions. Record the source archive SHA and a reproducible Git tree ID. Pre-existing BF-13 Rust gates have already passed; this slice changed only JS, docs and package scripts.

## Environment and audit

```bash
uname -a
node --version
which bwrap || true
ls -la /usr/bin/bwrap || true
bwrap --version || true
rustc -Vv
cargo --version
npm run test:bench
```

If `bwrap` is missing or fails under the host namespace policy, report **`BF14B_ENV_BLOCKED`** and the exact stderr/exit. **Do not** run Kroma on the bare host as a substitute or alter the signed gate flags. If the host can install bwrap securely, do so through its normal package manager, recording package version/provenance.

## Actual kernel preflight

```bash
mkdir -p /tmp/kroma-bf14b-validation
node scripts/bench-sandbox-supervisor.js probe \
  --out /tmp/kroma-bf14b-validation/probe-1
cat /tmp/kroma-bf14b-validation/probe-1/sandbox-probe.json
cat /tmp/kroma-bf14b-validation/probe-1/supervisor-receipt.json
```

**PASS requires every probe assertion to equal `true`, including mount/pid/net/user namespace divergence, host secret canary denial, write-denial, no default route, and nested child confinement; and the script must exit 0 without using host fallback.** A binary that merely prints a fabricated JSON record does not pass. Inspect actual bwrap invocation if necessary using approved host audit tools.

## Real isolated synthetic run (no API cost)

```bash
cargo build --release --locked -p kroma-cli
node scripts/bench-foundation.js freeze \
  --manifest experiments/foundation-v1/manifest.json \
  --suite experiments/foundation-v1/suite.json \
  --prices experiments/foundation-v1/prices.json \
  --out /tmp/kroma-bf14b-validation/frozen \
  --executor synthetic --budget-tokens 10000 --seed 12345
LOCK_SHA=$(sha256sum /tmp/kroma-bf14b-validation/frozen/lock.json | awk '{print $1}')
node scripts/bench-sandbox-supervisor.js synthetic \
  --lock /tmp/kroma-bf14b-validation/frozen/lock.json \
  --kroma target/release/kroma \
  --out /tmp/kroma-bf14b-validation/synthetic-run-1 \
  --timeout-ms 120000 --expect-lock-sha256 "$LOCK_SHA"
cat /tmp/kroma-bf14b-validation/synthetic-run-1/sandbox-receipt.json
```

Record whether Rust actually produced the expected task matrix, whether the report passed the independent Foundation verifier, and whether `sandbox-receipt.json` has `full_runner_wrapped_in_bwrap=true` / `measured_kernel_probe_passed=true` but `provider_execution_authorized=false` / `bf14a_job_isolation_proof=false`. These last two MUST remain false. Inspect that host-secret canary and external files cannot be written by the sandbox, and that temporary scratch files are removed on success and failure. Send logs with secret-bearing fields redacted. No Host Process fallback is acceptable.

### Negative tests

- Run the `synthetic` command against a frozen provider lock: must reject with `BF14B_PROVIDER_BLOCKED` **before any model call**, even if bwrap works.
- Set `--expect-lock-sha256` to 64 zeros: must refuse before launching anything.
- Tamper a frozen suite after `freeze` and rerun: must refuse.
- Run on a host without permitted user namespaces: must return `BF14B_ENV_BLOCKED`, never `BF14B_KERNEL_PREFLIGHT_PASS`.
- Confirm raw signed job artifacts in BF14A reject changed/missing/symlinked/oversized raw bytes and missing evidence root. All relevant Node tests must pass.

## Deliverable

Submit a short Markdown command-result summary with exact source identity, environment, exit codes, kernel preflight, isolated synthetic smoke matrix, artifacts and logged checks, plus a ZIP containing original logs and an SHA256SUMS manifest. Mark any blocked kernel step `ENV_BLOCKED` rather than reporting it as PASS. If the native sandbox command unexpectedly fails, **do not modify source or turn off isolation**: return the error and command for the code author to fix.
