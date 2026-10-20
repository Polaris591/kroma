# Kroma RC1: Two-pass signed release rehearsal (NO PUBLISH)

Status: **LOCAL CODE/TEST PATCH ONLY**; no GitHub Release workflow was run as part of this patch.
RC1 remains **RELEASE_BLOCKED** until the steps below actually succeed and platform evidence is reviewed.

## Trust boundary

- Only the project owner can sign an immutable candidate with the trusted Ed25519 release private key.
- The owner key stays on the owner machine in a mode-0600 file. **Do not upload or paste the private key.**
- The signed release-provenance document includes exactly nine file hashes: the six uniquely named
  executables, one `kroma-cli-V.tgz`, one runner-produced `sandbox-evidence.json`, and the complete
  `release-attestation.json` document. Its signature therefore binds sandbox status, source repository,
  source commit, run ID and attempt, package version, and the exact artifact set.
- The *unsigned* attestation is an observation, never permission to release. The verdict uses the
  pinned production trust root and GitHub's authenticated run/job metadata.
- No automatic publish job is configured. `ARTIFACT_SET_VERIFIED` is **not** `RELEASE_READY`.
- If the sandbox reports `HOST_CAPABILITY_BLOCKED`, the **sandbox GitHub job fails**, blocking the
  candidate before packing. The owner signer and final verdict independently reject such a status.
  Run on a capable trusted runner; never fake `PASS` or disable sandbox tests.

## Pass 1: create and inspect the candidate

Trigger `.github/workflows/release.yml` with `workflow_dispatch` leaving **both** input fields empty
(or run it from an explicitly approved `v*` tag). Do not publish or modify repository settings.
The workflow builds all targets, runs quality gates and real sandbox tests, gathers eight separately
named GitHub Actions artifacts, and creates **one** `release-candidate` bundle.

All generated files from the candidate are laid out as:

```text
release-candidate/
  candidate/
    kroma-linux-x64
    kroma-win32-x64.exe
    kroma-darwin-arm64
    kroma-darwin-x64
    kroma-android-arm64
    kroma-linux-arm64
    kroma-cli-1.5.0.tgz       # example; actual package.json version controls this name
    sandbox-evidence.json
  release-attestation.json
```

The attestation source must be the actual first-pass `GITHUB_REPOSITORY`, `GITHUB_SHA`, `GITHUB_RUN_ID`,
`GITHUB_RUN_ATTEMPT` and `needs.sandbox.outputs.status`. Build tool invocation:

```bash
node scripts/release-candidate.js stage --downloads raw-artifacts --out candidate --version 1.5.0
node scripts/release-attestation.js build --root candidate --version 1.5.0 \
  --commit '<40-char-git-sha>' --repo OWNER/REPO --run-id 123456789 --run-attempt 1 \
  --sandbox PASS --out release-attestation.json
node scripts/release-attestation.js verify release-attestation.json --root candidate
```

The above local commands illustrate the schema; **do not pass test data as release evidence**.

## Owner-only offline signing

Retrieve the exact first-pass artifact (outside the repository checkout):

```bash
mkdir -p /tmp/kroma-release-owner/release-candidate
GH_TOKEN="<YOUR_READ_ONLY_GITHUB_TOKEN>" gh run download '<SOURCE_RUN_ID>' \
  --repo OWNER/REPO --name release-candidate \
  --dir /tmp/kroma-release-owner/release-candidate
```

`GH_TOKEN` shown above is only a placeholder for the owner's local authentication; do not include
real tokens in scripts or chat. Review the GitHub run, job conclusions, source SHA and sandbox logs.
Never sign a run with a failed or capability-blocked sandbox.

```bash
node scripts/release-attestation.js verify \
  /tmp/kroma-release-owner/release-candidate/release-attestation.json \
  --root /tmp/kroma-release-owner/release-candidate/candidate
node scripts/release-provenance.js sign-candidate \
  --candidate-root /tmp/kroma-release-owner/release-candidate \
  --version 1.5.0 --key-id kroma-release-2026 \
  --private-key-file /secure/offline/kroma-ed25519.pem \
  --out /tmp/kroma-release-owner/signed-provenance.json
```

The `--out` path MUST be outside the candidate bundle. The signed JSON is public material; the private
key file must never leave its secured location.

## Pass 2: verify original bytes, not a rebuild

Use another `workflow_dispatch` invocation with **both** inputs:

- `candidate_run`: the numeric GitHub Actions run ID of pass 1.
- `provenance_b64`: base64 of the **signed public JSON** document
  (`base64 -w0 /tmp/kroma-release-owner/signed-provenance.json` on GNU/Linux).

GitHub accepts a short public document as a workflow_dispatch text input. The verifier validates the
source run via the GitHub API (expected repository, workflow path, commit, attempt, successful expected
jobs and one non-expired candidate), downloads **only** the original `release-candidate`, checks all
bytes and the signature, and returns `ARTIFACT_SET_VERIFIED` or `RELEASE_BLOCKED`. It does not rebuild.
The workflow uses explicit `contents: read` and `actions: read` permissions in the verification job.
The token only travels through the GitHub runner's own `GH_TOKEN`, not through user inputs.

Save both run links, full job conclusions, bundle hash/inventory, `release-verdict.json`, signature key
ID, and any platform-specific NOT_RUN status in the final independent acceptance report.

## Must remain blocked unless demonstrated

- A local Node pass is **not** an actual hosted GitHub Actions two-pass test.
- A successful GitHub workflow summary does not substitute for the signed candidate's actual sandbox
  result. `HOST_CAPABILITY_BLOCKED` stays blocked.
- Windows and Android native launch + trusted/untrusted TLS need separately sourced run evidence;
  no static build is a native runtime test.
- GitHub Actions artifacts preserve byte contents but may not preserve executable modes; never claim
  runtime verification solely from extracting a candidate.
- This workflow does **not** publish packages or create a GitHub Release.
