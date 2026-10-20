# RC1 security/integrity hotfix - local patch status (2026-10-18)

**Scope**: Correct the release attestation, signed verification, artifact collision/layout,
workflow_dispatch transport and input handling, without modifying the Rust agent runtime or T00-T30.

**This document supersedes older claims about `release-attestation.js` in the original RC1
acceptance documents. Those reports are preserved as historical evidence, not amended test logs.**

## Implemented changes

| Item | Change | Local verification |
| --- | --- | --- |
| RC1-R01 P0 | v2 attestation + sandbox evidence both covered by an owner Ed25519 provenance signature; blocked sandbox also fails the actual GitHub job; plus verified source run identity | Positive ephemeral-key test; status-only and evidence+attestation tamper tests fail closed |
| RC1-R02 P0 | Exact seven build artifacts plus sandbox evidence, downloaded separately, normalized to six collision-free binary names | Candidate inventory, missing/extra artifact and symlink tests |
| RC1-R03 P0 | Stable `candidate/` plus `release-attestation.json` bundle path; pass-2 explicitly expects same path | Full local bundle roundtrip and flattened/extra bundle rejection |
| RC1-R04 P0 | Offline owner-signed **public document** transferred as base64 `workflow_dispatch` input; source GitHub run queried and checked; no rebuild in pass 2 | Positive offline CLI signing/decoding/verification test; no GitHub live rehearsal |
| RC1-R05 P1 | Numeric run ID, env-only workflow inputs, no direct interpolation into Bash, least-privilege `actions:read` for verifier | YAML parsed; input pattern tests |
| RC1-R06 P1 | Delivered source archive, patch and external SHA manifest (this patch's output) | External hashes computed when archives are built |
| RC1-R07 P1 | GitHub live rehearsal, Android/Windows native runtime/TLS remain pending | **NOT_RUN / BLOCKED** |
| RC1-R08 P2 | Mandatory `--out` outside bundle for verdict; no generated `release-verdict.json` in source | No test residue created in checkout |
| RC1-R09 P2 | Removed cross-job `mtime` freshness heuristic; release candidate signed run/commit identity | Shell syntax check; full real cross-job run not yet tested |

## Acceptance snapshot

Node v22.16.0: upgrade 109/109, trust root 13/13, provenance 9/9, release pipeline
24/24, new release-attestation 24/24, ledger PASS. Actions, lock, OpenSSL and DAG static gates PASS.
The test count above reflects the independently executed local suites in this environment. It is **not**
Rust compilation, actual platform startup, a live GitHub Actions release, or a production private-key test.
Rust toolchain is unavailable in this environment; the earlier "995 passing" result is a source-package
log supplied by DeepSeek, and Rust sources in this hotfix are unchanged.

**Final state: `LOCAL_PATCH_VERIFIED`; `RELEASE_BLOCKED` pending a real two-pass GitHub Actions rehearsal,
valid owner-signed production release evidence, and explicit residual platform gates.**
See `docs/release/RC1_TWO_PASS_REHEARSAL.md` for the exact operator sequence.
