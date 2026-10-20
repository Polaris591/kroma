#!/usr/bin/env node
"use strict";

/**
 * T22 (KR-28, KR-29) — release/supply-chain gates.
 *
 * Each check pins one way a release can claim more than it verified: an archive extracted before its
 * digest was checked, artifacts from a different commit than the gates ran on, a platform quietly
 * missing, a pack produced after a failing gate, a changed dependency graph, a manifest that no
 * longer matches the bytes, or an SBOM that cannot be recomputed.
 *
 * Run: node scripts/test-release-pipeline.js
 */

const assert = require("node:assert");
const crypto = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");
const { execFileSync } = require("node:child_process");

const {
  verifyPinnedArchive,
  sha256File,
  assertSameCommit,
  assertQualityGates,
  assertAllPlatformsPresent,
  assertDependencyLockUnchanged,
  rebuildIdentity,
  assertManifestMatchesArtifacts,
  assertActionsPinned,
  assertOpenSslVerified,
  assertReleaseDag,
  ReleaseGateError
} = require(path.join(__dirname, "release-gate.js"));
const { buildSbom, parseLockfile } = require(path.join(__dirname, "generate-sbom.js"));

const ROOT = path.join(__dirname, "..");
let checks = 0;

function ok(name, fn) {
  try {
    fn();
    checks += 1;
    console.log(`  ok  ${name}`);
  } catch (err) {
    console.error(`  FAIL ${name}\n       ${err && err.message}`);
    process.exitCode = 1;
  }
}

function refuses(fn, needle) {
  try {
    fn();
  } catch (err) {
    assert.ok(err instanceof ReleaseGateError, `expected a ReleaseGateError, got ${err}`);
    assert.ok(
      String(err.message).includes(needle),
      `expected ${JSON.stringify(needle)} in ${JSON.stringify(err.message)}`
    );
    return;
  }
  assert.fail(`expected a refusal containing ${JSON.stringify(needle)}`);
}

const sha = (text) => crypto.createHash("sha256").update(text).digest("hex");

console.log("tampered_source_archive_fails_before_extract");
ok("a matching digest passes", () => {
  const digest = sha("openssl-3.5.7.tar.gz bytes");
  assert.strictEqual(
    verifyPinnedArchive({ name: "openssl-3.5.7.tar.gz", expected: digest, actual: digest }),
    true
  );
});

ok("a tampered archive is refused before extraction", () => {
  const pinned = sha("openssl-3.5.7.tar.gz bytes");
  const tampered = sha("openssl-3.5.7.tar.gz bytes with a backdoor");
  refuses(
    () => verifyPinnedArchive({ name: "openssl", expected: pinned, actual: tampered }),
    "does not match the pinned"
  );
});

ok("no pin in the repository is a configuration error, not a pass", () => {
  refuses(
    () => verifyPinnedArchive({ name: "openssl", expected: "", actual: sha("x") }),
    "no pinned sha256"
  );
  refuses(
    () => verifyPinnedArchive({ name: "openssl", expected: undefined, actual: sha("x") }),
    "no pinned sha256"
  );
});

ok("every workflow fetches OpenSSL through the verify-before-extract script", () => {
  // RC1-04: the ordering check now lives inside scripts/fetch-openssl.sh, so the workflow-level
  // assertion is that no workflow extracts an OpenSSL archive on its own and that the pin is declared.
  for (const file of [".github/workflows/release.yml", ".github/workflows/ci.yml"]) {
    const workflow = fs.readFileSync(path.join(ROOT, file), "utf8");
    assert.ok(
      workflow.includes("scripts/fetch-openssl.sh"),
      `${file} must fetch OpenSSL through the shared script`
    );
    assert.ok(
      /^\s*OPENSSL_SHA256:/m.test(workflow),
      `${file} must declare the pin the script verifies against`
    );
    assert.ok(
      !/^\s*tar\s+-?[a-z]*x[a-z]*f?\s+.*openssl[^ ]*\.tar/m.test(workflow),
      `${file} must not extract a downloaded OpenSSL archive directly`
    );
  }
});

console.log("release_uses_same_commit_for_tests_and_artifacts");
ok("the same commit passes", () => {
  const commit = "a".repeat(40);
  assert.strictEqual(assertSameCommit({ testedCommit: commit, artifactCommit: commit }), true);
});

ok("artifacts from another commit are refused", () => {
  refuses(
    () => assertSameCommit({ testedCommit: "a".repeat(40), artifactCommit: "b".repeat(40) }),
    "a release must use one commit"
  );
  // A short sha is not evidence of identity.
  refuses(
    () => assertSameCommit({ testedCommit: "abc1234", artifactCommit: "abc1234" }),
    "not a full 40-character sha"
  );
});

console.log("release_quality_failure_blocks_pack_and_sign");
ok("all gates passing allows packing", () => {
  assert.strictEqual(
    assertQualityGates({ fmt: "pass", clippy: "pass", test: "pass", crossBuild: "pass" }),
    true
  );
});

ok("any failing gate blocks pack and sign", () => {
  for (const failing of ["fmt", "clippy", "test", "crossBuild"]) {
    const gates = { fmt: "pass", clippy: "pass", test: "pass", crossBuild: "pass" };
    gates[failing] = "fail";
    refuses(() => assertQualityGates(gates), "packing and signing are blocked");
  }
  // A skipped gate is not a pass either.
  refuses(
    () => assertQualityGates({ fmt: "pass", clippy: "pass", test: "skipped", crossBuild: "pass" }),
    "packing and signing are blocked"
  );
});

console.log("missing_platform_artifact_cannot_publish");
ok("every declared platform present allows publishing", () => {
  const required = ["linux-x64", "linux-arm64", "win32-x64"];
  assert.strictEqual(assertAllPlatformsPresent({ required, produced: required }), true);
});

ok("a missing platform blocks the release", () => {
  refuses(
    () =>
      assertAllPlatformsPresent({
        required: ["linux-x64", "darwin-arm64"],
        produced: ["linux-x64"]
      }),
    "missing platform artifact"
  );
  // An artifact nobody declared is also refused: it would ship without a gate.
  refuses(
    () => assertAllPlatformsPresent({ required: ["linux-x64"], produced: ["linux-x64", "freebsd"] }),
    "unexpected platform artifact"
  );
});

console.log("cargo_lock_change_requires_review");
ok("the checked-in lock matches its pinned digest", () => {
  const pinnedFile = path.join(ROOT, "docs/release/Cargo.lock.sha256");
  assert.ok(fs.existsSync(pinnedFile), "the lock digest must be pinned in the repository");
  const pinned = fs.readFileSync(pinnedFile, "utf8").trim().split(/\s+/)[0];
  const actual = sha256File(path.join(ROOT, "Cargo.lock"));
  assert.strictEqual(assertDependencyLockUnchanged({ pinned, actual }), true);
});

ok("a changed lock is refused (and so is a missing pin)", () => {
  refuses(
    () => assertDependencyLockUnchanged({ pinned: "a".repeat(64), actual: "b".repeat(64) }),
    "Cargo.lock changed"
  );
  refuses(
    () => assertDependencyLockUnchanged({ pinned: "", actual: "b".repeat(64) }),
    "no pinned digest"
  );
});

console.log("rebuild_identity_is_comparable");
ok("the same artifacts produce the same identity regardless of order", () => {
  const artifacts = [
    { name: "kroma-linux-x64", sha256: "a".repeat(64), length: 10 },
    { name: "kroma-win32-x64", sha256: "b".repeat(64), length: 20 }
  ];
  const forward = rebuildIdentity(artifacts);
  const backward = rebuildIdentity([...artifacts].reverse());
  assert.strictEqual(forward, backward);
  assert.strictEqual(forward.length, 64);
});

ok("any byte or length change changes the identity", () => {
  const base = [{ name: "kroma", sha256: "a".repeat(64), length: 10 }];
  const identity = rebuildIdentity(base);
  assert.notStrictEqual(
    rebuildIdentity([{ name: "kroma", sha256: "c".repeat(64), length: 10 }]),
    identity
  );
  assert.notStrictEqual(rebuildIdentity([{ name: "kroma", sha256: "a".repeat(64), length: 11 }]), identity);
});

ok("an artifact without a digest cannot contribute to an identity", () => {
  refuses(() => rebuildIdentity([{ name: "kroma", sha256: "", length: 10 }]), "has no sha256");
  refuses(() => rebuildIdentity([{ name: "", sha256: "a".repeat(64), length: 10 }]), "has no name");
});

console.log("release artifacts and the signed manifest agree");
ok("matching manifest passes", () => {
  const artifacts = [{ name: "kroma-cli.tgz", sha256: "a".repeat(64), length: 100 }];
  assert.strictEqual(
    assertManifestMatchesArtifacts({
      manifest: { version: "1.5.0", artifacts: [{ name: "kroma-cli.tgz", sha256: "a".repeat(64), length: 100, version: "1.5.0" }] },
      artifacts
    }),
    true
  );
});

ok("a manifest for different bytes is refused", () => {
  const artifacts = [{ name: "kroma-cli.tgz", sha256: "a".repeat(64), length: 100 }];
  refuses(
    () =>
      assertManifestMatchesArtifacts({
        manifest: { version: "1.5.0", artifacts: [{ name: "kroma-cli.tgz", sha256: "b".repeat(64), length: 100, version: "1.5.0" }] },
        artifacts
      }),
    "manifest sha256"
  );
  refuses(
    () =>
      assertManifestMatchesArtifacts({
        manifest: { version: "1.5.0", artifacts: [{ name: "kroma-cli.tgz", sha256: "a".repeat(64), length: 100, version: "1.4.0" }] },
        artifacts
      }),
    "artifact version"
  );
  refuses(
    () => assertManifestMatchesArtifacts({ manifest: { version: "1.5.0", artifacts: [] }, artifacts }),
    "lists no artifacts"
  );
});

console.log("SBOM is recomputable");
ok("the SBOM is deterministic and every component is pinned", () => {
  const lockText = fs.readFileSync(path.join(ROOT, "Cargo.lock"), "utf8");
  const packageJsonText = fs.readFileSync(path.join(ROOT, "package.json"), "utf8");
  const first = buildSbom({ lockText, packageJsonText, toolchain: "1.85.1" });
  const second = buildSbom({ lockText, packageJsonText, toolchain: "1.85.1" });
  assert.strictEqual(JSON.stringify(first), JSON.stringify(second), "the SBOM must be reproducible");
  assert.ok(first.componentCount > 50, `expected a real dependency set, got ${first.componentCount}`);
  assert.strictEqual(first.product.version, JSON.parse(packageJsonText).version);
  for (const component of first.components) {
    assert.ok(component.name && component.version, "every component needs a name and version");
    if (component.source && component.source.startsWith("registry+")) {
      assert.ok(/^[0-9a-f]{64}$/.test(component.checksum || ""), `${component.name} has no checksum`);
    }
  }
  // The lockfile digest binds the SBOM to the graph it describes.
  assert.strictEqual(first.lockfileSha256, sha(lockText));
});

ok("a registry crate without a checksum is refused", () => {
  const broken = '[[package]]\nname = "x"\nversion = "1.0.0"\nsource = "registry+https://example.invalid"\n';
  assert.throws(
    () => buildSbom({ lockText: broken, packageJsonText: '{"name":"k","version":"1"}', toolchain: "t" }),
    /no checksum/
  );
});

console.log("third-party actions are pinned");
ok("an unpinned action reference is detected", () => {
  refuses(
    () => assertActionsPinned("      - uses: actions/checkout@v4\n"),
    "must be pinned to a 40-character commit sha"
  );
  refuses(
    () => assertActionsPinned("      - uses: actions/checkout\n"),
    "must be pinned to a 40-character commit sha"
  );
  // Local composite actions are fine: they are this repository's own code.
  assert.strictEqual(assertActionsPinned("      - uses: ./.github/actions/build\n"), 1);
  // A commit-pinned reference passes.
  assert.strictEqual(
    assertActionsPinned(`      - uses: actions/checkout@${"a".repeat(40)}\n`),
    1
  );
  // The conventional pinned-with-comment form passes: the trailing `# v4` is documentation, not part
  // of the reference. Treating it as part of the pin would push maintainers to drop the comment.
  assert.strictEqual(
    assertActionsPinned(`      - uses: actions/checkout@${"a".repeat(40)} # v4\n`),
    1
  );
  assert.strictEqual(
    assertActionsPinned(`      - uses: actions/cache@${"b".repeat(40)} # v4.0.2 (node20)\n`),
    1
  );
  // The comment cannot rescue an unpinned reference.
  refuses(
    () => assertActionsPinned("      - uses: actions/checkout@v4 # looks pinned\n"),
    "must be pinned to a 40-character commit sha"
  );
  // Nor can a comment hide a short pin.
  refuses(
    () => assertActionsPinned(`      - uses: actions/checkout@${"a".repeat(39)} # v4\n`),
    "must be pinned to a 40-character commit sha"
  );
});

// Report (do not fail on) the repository's current pinning state: the owner has to supply the
// reviewed commit SHAs, and the acceptance document records this as an open release-gate item.
ok("the repository's current action pinning state is reported", () => {
  const workflow = fs.readFileSync(path.join(ROOT, ".github/workflows/release.yml"), "utf8");
  let pinned = 0;
  let unpinned = 0;
  for (const line of workflow.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed.startsWith("- uses:") && !trimmed.startsWith("uses:")) continue;
    // Strip a trailing `# v4` comment the same way the gate does, so the report matches reality.
    const reference = trimmed.replace(/(^|\s)#.*$/, "").replace(/^(-\s*)?uses:\s*/, "").trim();
    if (reference.startsWith("./") || reference.startsWith(".github/")) continue;
    const pin = reference.slice(reference.lastIndexOf("@") + 1);
    if (/^[0-9a-f]{40}$/.test(pin)) pinned += 1;
    else unpinned += 1;
  }
  console.log(
    `       release.yml: ${pinned} pinned, ${unpinned} on mutable tags (owner must supply reviewed SHAs)`
  );
  assert.ok(pinned + unpinned > 0, "the workflow should reference at least one action");
});

console.log("RC1-04: OpenSSL sources are verified before extraction");
ok("a missing or malformed OpenSSL pin fails closed before any download", () => {
  const script = path.join(ROOT, "scripts", "fetch-openssl.sh");
  const run = (args) => {
    try {
      execFileSync("bash", [script, ...args], { stdio: "pipe" });
      return { code: 0, out: "" };
    } catch (err) {
      return { code: err.status, out: String(err.stderr || "") };
    }
  };
  const missing = run(["3.5.7", ""]);
  assert.strictEqual(missing.code, 3);
  assert.match(missing.out, /OPENSSL_SHA256 is not set/);

  const short = run(["3.5.7", "a".repeat(63)]);
  assert.strictEqual(short.code, 3);
  assert.match(short.out, /exactly 64 characters/);

  const notHex = run(["3.5.7", "Z".repeat(64)]);
  assert.strictEqual(notHex.code, 3);
  assert.match(notHex.out, /64 lowercase hex/);

  // No version is a usage error, not a silent no-op.
  assert.strictEqual(run(["", "a".repeat(64)]).code, 2);
});

ok("a direct extraction of a downloaded OpenSSL archive is refused", () => {
  const bad = [
    "      - run: |",
    "          curl -fsSL -o openssl.tar.gz https://example.invalid/openssl.tar.gz",
    "          tar -xzf openssl.tar.gz",
  ].join("\n");
  refuses(() => assertOpenSslVerified(bad, "fixture.yml"), "directly instead of using scripts/fetch-openssl.sh");

  // The shared path is accepted, and the pin must be declared somewhere in the workflow.
  const good = [
    "env:",
    "  OPENSSL_SHA256: '<pinned>'",
    "      - run: scripts/fetch-openssl.sh \"${OPENSSL_VERSION}\" \"${OPENSSL_SHA256}\" .",
  ].join("\n");
  assert.ok(assertOpenSslVerified(good, "fixture.yml") > 0);

  const noPin = "      - run: scripts/fetch-openssl.sh \"${OPENSSL_VERSION}\" \"${OPENSSL_SHA256}\" .";
  refuses(() => assertOpenSslVerified(noPin, "fixture.yml"), "never declares OPENSSL_SHA256");
});

console.log("RC1-03: the release DAG gates artifacts on quality");
ok("packing without a quality job is refused", () => {
  const noQuality = [
    "jobs:",
    "  pack:",
    "    needs: [linux, windows]",
  ].join("\n");
  refuses(() => assertReleaseDag(noQuality), "no quality job");

  const ungated = [
    "jobs:",
    "  quality:",
    "    runs-on: ubuntu-latest",
    "  pack:",
    "    needs: [linux, windows]",
  ].join("\n");
  refuses(() => assertReleaseDag(ungated), "pack does not depend on the quality job");

  // The real workflow must pass, so this test cannot drift from the shipped file.
  const real = fs.readFileSync(path.join(ROOT, ".github/workflows/release.yml"), "utf8");
  const jobs = assertReleaseDag(real);
  assert.ok(jobs.includes("quality"));
  assert.ok(jobs.includes("pack"));
  // No publish job exists yet, and that is the safe default: publishing is an owner decision.
  assert.strictEqual(jobs.includes("publish"), false);
});

console.log("");
if (process.exitCode) {
  console.error(`release pipeline suite FAILED after ${checks} passing check(s)`);
} else {
  console.log(`all ${checks} checks passed`);
}
