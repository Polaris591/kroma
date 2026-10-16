# Kroma Release Manifest Specification & Migration Guide (B14 / F13)

## 1. Executive Summary

This document specifies the Ed25519 signed release manifest schema, cryptographic verification rules, fail-closed trust root design, pre-extraction archive security guards, and client migration strategy for Kroma distribution artifacts (`bin/kroma.js` and `kroma-cli`).

Addressing vulnerability **F13** ("Download lacks independent trust anchor; execution proceeds when checksum missing"), Kroma removes all silent unauthenticated HTTP/checksum fallbacks. Any binary or package update requires cryptographic authentication against client-pinned trusted public keys before artifact extraction or execution.

---

## 2. Manifest Schema & Serialization

The release manifest is delivered as a JSON document (bounded to 64 KB, `MAX_MANIFEST_BYTES = 65536`).

### 2.1 Target-Specific Manifest URL Convention

To prevent multi-architecture deadlocks across disparate platforms, manifests are published per target:
`https://cobblemon.asia/manifests/kroma-manifest-${target}.json`

Supported targets (`ALLOWED_TARGETS`):
- `cli`: Generic npm CLI package tarball (used by `kroma upgrade`)
- `linux-x64`, `linux-arm64`, `linux-arm`
- `darwin-x64`, `darwin-arm64`
- `win32-x64`, `win32-arm64`
- `android-arm64`

### 2.2 Manifest Fields

| Field | Type | Description / Constraints |
| :--- | :--- | :--- |
| `schemaVersion` | `integer` | Strictly `1`. Reject any unknown schema versions. |
| `keyId` | `string` | Identifier of the signing key (`^[A-Za-z0-9_-]{1,64}$`). |
| `version` | `string` | Target release version. Strict SemVer 2.0.0 (no leading zeros in numeric components). |
| `target` | `string` | Platform identifier matching `ALLOWED_TARGETS`. |
| `tarballUrl` | `string` | HTTPS URL pointing to the release archive. Host must be on `ALLOWED_TARBALL_HOSTS`. |
| `sha256` | `string` | Hexadecimal SHA-256 digest of the compressed tarball (`^[0-9a-f]{64}$`). |
| `length` | `integer` | Exact byte length of the compressed tarball (`1 <= length <= 200MB`). |
| `expires` | `integer` | Expiration unix timestamp in seconds (positive safe integer). |
| `antirollback` | `string` | Minimum permitted version floor. Strict SemVer 2.0.0. |
| `signature` | `string` | Canonical 88-character base64 string encoding 64-byte Ed25519 signature. |

### 2.3 Canonical Signing Payload

To prevent JSON parser ambiguities, key reordering, duplicate key collision attacks, and numeric formatting inconsistencies, Ed25519 signatures are **not** computed over raw JSON bytes. Instead, signatures are computed over deterministic UTF-8 bytes:

```text
kroma-signed-manifest:v1\n
antirollback:<antirollback>\n
expires:<expires>\n
keyId:<keyId>\n
length:<length>\n
schemaVersion:<schemaVersion>\n
sha256:<sha256>\n
tarballUrl:<tarballUrl>\n
target:<target>\n
version:<version>\n
```

Key invariant rules:
1. **Domain Separation**: All payloads begin with `kroma-signed-manifest:v1\n` to prevent cross-protocol / cross-context signature replay.
2. **Alphabetical Ordering**: Fields are sorted strictly alphabetically.
3. **Control Character Rejection**: All string fields are validated to contain no control characters (`\x00-\x1f\x7f`), CR, LF, or unescaped Unicode spaces.
4. **Canonical Base64 Signature**: The 64-byte Ed25519 signature is encoded in standard base64 (`^[A-Za-z0-9+/]{86}==$`). The decoding is checked to ensure no uncanonical padding bits or non-standard characters exist.

---

## 3. Strict Parser & Duplicate Key Guards

Standard JavaScript `JSON.parse` silently overwrites duplicate keys (e.g. `{"version": "1.5.1", "version": "1.5.0"}`). Furthermore, Unicode escape sequences such as `\u0076ersion` can be used to bypass simplistic string-based or regex-based key detectors.

`bin/kroma.js` includes `parseStrictJson()`:
- Scans JSON syntax token-by-token.
- Normalizes Unicode escape sequences prior to property name registration.
- Uses an exact set of seen keys per object scope. Any duplicate key causes an immediate fatal error: `Duplicate key in object: <key>`.
- Rejects trailing commas in objects and arrays.
- Rejects unescaped control characters in string literals.
- Rejects numbers with leading zeros (preventing octal confusion).

---

## 4. Trust Root Architecture & Fail-Closed Policy

### 4.1 Production Release Trust Root: Explicitly Unconfigured

In `bin/kroma.js`:
```javascript
const PRODUCTION_TRUSTED_KEYS = Object.freeze({});
```
Because no official production keypair has been generated and provisioned by human operators, `PRODUCTION_TRUSTED_KEYS` is intentionally empty.

**Fail-Closed Behavior**:
- When `PRODUCTION_TRUSTED_KEYS` is empty, online self-upgrade (`kroma upgrade`) and on-demand binary download (`downloadBinaryIfNeeded`) are **strictly blocked**.
- Error message: `● upgrade ✗ production release trust root is unconfigured; self-upgrade is blocked.`
- **Release Status**: Clearly marked as **RELEASE BLOCKED**. Kroma does not falsely claim production readiness for automated online upgrades until operators generate and configure the production trust root.
- **Local Binary Execution Unaffected**: If native pre-built binaries are already bundled locally (`bin/vendor/...`), running `kroma` or `kroma --version` continues to execute normally without touching the network.

### 4.2 Test Fixture Key Injection

To maintain strict isolation between test fixtures and production security:
- `PRODUCTION_TRUSTED_KEYS` is frozen in code and cannot be modified or overridden via environment variables or remote config files.
- Pure functions (`verifyManifest`, `downloadBinaryIfNeeded`, `handleSelfUpgrade`) accept an optional `options.trustedKeys` parameter passed explicitly by in-process JavaScript test runners.
- CLI invocations through `main` always bind to `PRODUCTION_TRUSTED_KEYS`.

---

## 5. Security Invariants & Attack Mitigations

| Attack Vector | Defense Mechanism in B14 | Test Verification |
| :--- | :--- | :--- |
| **Mirror Compromise (MITM / CDN Tamper)** | Tarball SHA-256 and byte length are signed by Ed25519 private key. `handleSelfUpgrade` downloads the archive to local staging and validates SHA-256 + length before spawning `npm`. Corrupted or substituted tarballs fail and npm is never spawned. | `handleSelfUpgrade` and `downloadBinaryIfNeeded` tested with tampered payloads. |
| **Direct URL Install Bypass** | `handleSelfUpgrade` passes the verified local archive path to `npm install -g <archive>`, never a remote URL. | Tested in regression suite with mock spawn. |
| **Version Downgrade Attack** | 1. Candidate `version` must be `>=` local installed version.<br>2. Candidate `version` must be `>=` manifest `antirollback` floor.<br>3. Manifest cannot lower local version baseline.<br>4. Fail-closed: unparseable SemVer comparisons immediately abort. | Tested with SemVer edge cases (1.10.0 > 1.9.0, prereleases, invalid numbers). |
| **Tarball Traversal & Symlink Attacks** | Prior to extraction or package installation, `inspectAndExtractTarStream` inspects 512-byte binary tar headers directly from the gunzip stream in pure Node (no external `tar` dependency or verbose regex). Rejects symlinks, hardlinks, FIFOs, devices, PAX headers, GNU longnames, base-256 sizes, absolute paths, `..` traversals, Windows drives, colons/ADS, backslashes, UNC paths, trailing dots/spaces, control characters, and duplicate paths. | 25 synthetic attack archives tested in regression suite. |
| **Decompression Resource Exhaustion (Zip Bombs)** | Enforces strict bounded limits: `MAX_UNCOMPRESSED_BYTES = 500MB` and `MAX_ARCHIVE_ENTRIES = 10000`. Exceeding limits immediately destroys stream and unlinks temp files. | Tested with byte and entry count limit triggers. |
| **Extraction Hang / DoS Attacks** | Pure-Node single binary extraction operates with an absolute `deadlineMs` (default 30s), exclusive temp file creation (`flags: "wx"`, `mode: 0o755`), and atomic `renameSync`. Does not invoke external `tar`. | Tested with hung streams and missing targets. |
| **Malicious Redirects** | Both `fetchText` and `downloadTarballWithIntegrity` validate every redirect hop: scheme must be HTTPS, host must reside on `ALLOWED_TARBALL_HOSTS`, no port, no userinfo. Max 3 hops. | Rejects non-allow-listed hosts and HTTP schemes. |
| **Slow Stream / Hung Server DoS** | Overall absolute deadline timer (`deadlineMs`) covers the entire operation, including redirects and slow chunks. Sockets are destroyed on deadline. | Tested in regression suite. |
| **Duplicate Key Parser Confusion** | `parseStrictJson` rejects any duplicate key, including Unicode-escaped variations (`\u0076ersion`). | Tested against Unicode escapes and duplicated property keys. |
| **Incomplete / Corrupted Installation** | Extraction is performed to an isolated temporary directory. File type is verified to be a regular file (not symlink). Permissions set to 0755. Staged into destination directory and installed via atomic `fs.renameSync`. | Tested end-to-end with fixture tarball. |

---

## 6. Manifest Generation Tool & CI Release Specification

### 6.1 Manifest Generation Script (`scripts/generate-release-manifest.js`)

A dedicated offline/CI script generates signed manifests using the identical canonical logic as the client:
```bash
node scripts/generate-release-manifest.js \
  --target <target> \
  --version <semver> \
  --tarball <path-to-archive.tgz> \
  --url <https://tarball-url> \
  --key-id <key-id> \
  --private-key-file <path-to-private-key.pem> \
  [--antirollback <version>] \
  [--ttl-days <days>] \
  [--out <output-manifest.json>]
```

Security design of the generation tool:
1. **No Argv Secret Interpolation**: Private keys are strictly loaded from a protected file path via `--private-key-file`. Passing private keys directly on the command line or environment variables is forbidden to prevent leakage into process tables and CI logs.
2. **Streaming Hashes**: Computes SHA-256 and byte length via bounded streams, avoiding loading large archives into memory.
3. **Atomic File Output**: Checks that `--out` is not a symlink, writes to a sibling temporary file, and atomically renames with `0o644` permissions.

### 6.2 CI Release Workflow Integration (Future Implementation)

When human operators provision production signing keys:
1. The private key must be provided via a secure runner mechanism (e.g. GitHub Actions secret written to a temporary restricted-permission file `0600` or an external HSM / KMS signing agent).
2. Manifest generation must be a **required, non-optional step** in `.github/workflows/release.yml`. If signing keys are absent, the release workflow must fail hard (no silent unsigned releases).
3. The generated manifests (`kroma-manifest-${target}.json`) and release archives must be uploaded together as release assets and mirrored to `https://cobblemon.asia/manifests/`.

**Current Release Status**: Until official production Ed25519 keys are provisioned, online self-update remains **RELEASE BLOCKED**.

---

## 7. Migration Roadmap for Legacy Clients

### 7.1 Transition Strategy

1. **Dual Publishing on Mirrors**:
   - The release mirror continues publishing legacy `kroma-version.json` for older v1.5.0 clients that lack manifest verification logic.
   - The mirror concurrently publishes signed target-specific manifests (`kroma-manifest-${target}.json`) for new v1.5.1+ clients.
2. **Zero Unsigned Fallback for Modern Clients**:
   - New clients (>= 1.5.1) will **never** silently fall back to `kroma-version.json` or unverified downloads. If manifest verification fails or is absent, the upgrade fails closed.
3. **Bootstrapping Legacy Clients**:
   - Upgrading from legacy <= 1.5.0 to 1.5.1+ cannot be securely achieved via an unauthenticated mirror.
   - Operators must instruct users to upgrade via authenticated package managers (e.g., `npm install -g kroma-cli` using npm's registry integrity) or manually download verified release tarballs.

### 7.2 Residual Risks & Security Boundaries

1. **Local System Clock Reliance**: Manifest expiration (`expires`) relies on the client's local system clock. A host whose clock is skewed backwards could accept an expired manifest within its validity period.
2. **Root Key Compromise**: Ed25519 signatures authenticate releases against configured trust roots. If an operator's private signing key is compromised, attackers could sign malicious manifests. Root key rotation requires shipping a new client version with an updated trust root or an M-of-N threshold signing design.
3. **Antirollback Floor**: Antirollback is bound by the installed client's version in `package.json`. A freshly installed client without prior persistent state relies on its built-in version floor.
