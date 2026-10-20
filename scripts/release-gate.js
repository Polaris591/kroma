#!/usr/bin/env node
"use strict";

/**
 * T22 (KR-28, KR-29): release/supply-chain gates.
 *
 * The rule this encodes: a release may only be published from source that already passed the same
 * gates, with every third-party input verified before use, and with a hash manifest that can be
 * recomputed. Every function here is pure so the regressions can be driven from a test without a
 * release run; `main()` exposes the same checks as a CLI for CI.
 *
 * Deliberately NOT encoded here: a real signature. That needs the owner's key (T21, BLOCKED). These
 * gates run *before* signing and cannot substitute for it.
 */

const crypto = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");

class ReleaseGateError extends Error {}

function fail(message) {
  throw new ReleaseGateError(message);
}

/**
 * A downloaded source archive must be verified against a pinned digest **before** it is extracted.
 *
 * `expected` is the digest recorded in the repository; `actual` is what arrived. An empty or
 * missing expected digest is a configuration error, not a pass: "no pin" is how a tampered archive
 * gets extracted.
 */
function verifyPinnedArchive({ name, expected, actual }) {
  if (typeof expected !== "string" || !/^[0-9a-f]{64}$/.test(expected)) {
    fail(
      `${name}: no pinned sha256 in the repository; a release may not extract an unverified archive`
    );
  }
  if (typeof actual !== "string" || !/^[0-9a-f]{64}$/.test(actual)) {
    fail(`${name}: the downloaded archive has no computable digest`);
  }
  if (expected !== actual) {
    fail(`${name}: archive digest ${actual} does not match the pinned ${expected}`);
  }
  return true;
}

/** sha256 of a file, streaming (archives are not buffered). */
function sha256File(file) {
  const hash = crypto.createHash("sha256");
  const fd = fs.openSync(file, "r");
  try {
    const buffer = Buffer.allocUnsafe(1024 * 1024);
    let read = 0;
    while ((read = fs.readSync(fd, buffer, 0, buffer.length, null)) > 0) {
      hash.update(buffer.subarray(0, read));
    }
  } finally {
    fs.closeSync(fd);
  }
  return hash.digest("hex");
}

/** The artifacts must come from the commit the gates ran on. */
function assertSameCommit({ testedCommit, artifactCommit }) {
  const isSha = (value) => typeof value === "string" && /^[0-9a-f]{40}$/.test(value);
  if (!isSha(testedCommit)) {
    fail(`the tested commit is not a full 40-character sha: ${testedCommit}`);
  }
  if (!isSha(artifactCommit)) {
    fail(`the artifact commit is not a full 40-character sha: ${artifactCommit}`);
  }
  if (testedCommit !== artifactCommit) {
    fail(
      `the artifacts were built from ${artifactCommit} but the gates ran on ${testedCommit}; a release must use one commit`
    );
  }
  return true;
}

/** A quality gate failure blocks pack and sign. */
function assertQualityGates({ fmt, clippy, test, crossBuild }) {
  const gates = { fmt, clippy, test, crossBuild };
  const failed = Object.keys(gates).filter((key) => gates[key] !== "pass");
  if (failed.length > 0) {
    fail(`packing and signing are blocked: ${failed.join(", ")} did not pass`);
  }
  return true;
}

/** Every declared platform needs an artifact; a missing one blocks publishing that release. */
function assertAllPlatformsPresent({ required, produced }) {
  const missing = required.filter((platform) => !produced.includes(platform));
  if (missing.length > 0) {
    fail(
      `missing platform artifact(s): ${missing.join(", ")}; a release may not publish without them ` +
        `(drop the platform explicitly instead of shipping a stub)`
    );
  }
  const unexpected = produced.filter((platform) => !required.includes(platform));
  if (unexpected.length > 0) {
    fail(`unexpected platform artifact(s): ${unexpected.join(", ")}`);
  }
  return true;
}

/**
 * A changed dependency lock is a review event.
 *
 * `pinned` is the digest recorded in the repository for `Cargo.lock`; a mismatch means the
 * dependency graph changed and the cache/evidence from the previous lock no longer describes this
 * build.
 */
function assertDependencyLockUnchanged({ pinned, actual }) {
  if (typeof pinned !== "string" || !/^[0-9a-f]{64}$/.test(pinned)) {
    fail("Cargo.lock has no pinned digest; a dependency change would pass unnoticed");
  }
  if (pinned !== actual) {
    fail(
      `Cargo.lock changed (${pinned} -> ${actual}); this needs review and a re-run of every gate`
    );
  }
  return true;
}

/**
 * A rebuild identity: the sorted `name:sha256:length` triples, hashed. Two builds of the same source
 * with the same toolchain produce the same value; anything else means the build is not reproducible.
 */
function rebuildIdentity(artifacts) {
  const lines = artifacts
    .map((artifact) => {
      if (!artifact || typeof artifact.name !== "string" || artifact.name.length === 0) {
        fail("an artifact has no name");
      }
      if (!/^[0-9a-f]{64}$/.test(artifact.sha256 || "")) {
        fail(`artifact ${artifact.name} has no sha256`);
      }
      if (!Number.isInteger(artifact.length) || artifact.length <= 0) {
        fail(`artifact ${artifact.name} has no length`);
      }
      return `${artifact.name}:${artifact.sha256}:${artifact.length}`;
    })
    .sort();
  return crypto.createHash("sha256").update(lines.join("\n")).digest("hex");
}

/**
 * The signed manifest and the artifacts must agree.
 *
 * This is the check that catches "we signed a manifest for a tarball we then rebuilt".
 */
function assertManifestMatchesArtifacts({ manifest, artifacts }) {
  const byName = new Map(artifacts.map((artifact) => [artifact.name, artifact]));
  const entries = Array.isArray(manifest.artifacts) ? manifest.artifacts : [];
  if (entries.length === 0) {
    fail("the manifest lists no artifacts");
  }
  for (const entry of entries) {
    const artifact = byName.get(entry.name);
    if (!artifact) {
      fail(`the manifest names ${entry.name}, which was not produced`);
    }
    if (artifact.sha256 !== entry.sha256) {
      fail(`${entry.name}: manifest sha256 ${entry.sha256} != artifact ${artifact.sha256}`);
    }
    if (artifact.length !== entry.length) {
      fail(`${entry.name}: manifest length ${entry.length} != artifact ${artifact.length}`);
    }
    if (entry.version !== manifest.version) {
      fail(`${entry.name}: artifact version ${entry.version} != manifest version ${manifest.version}`);
    }
  }
  return true;
}

/**
 * Third-party actions must be pinned to a commit, not a mutable tag.
 *
 * A `@v4` reference is a moving target: the code that runs in a release job can change without any
 * commit in this repository.
 */
function assertActionsPinned(workflowText) {
  const usesLines = workflowText
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line.startsWith("- uses:") || line.startsWith("uses:"));
  const unpinned = [];
  for (const line of usesLines) {
    // `owner/action@<sha> # v4` is the conventional pinned form: the comment keeps the human-readable
    // version and must not be treated as part of the reference.
    const withoutComment = line.replace(/(^|\s)#.*$/, "");
    const reference = withoutComment.replace(/^(-\s*)?uses:\s*/, "").trim();
    if (reference.startsWith("./") || reference.startsWith(".github/")) continue;
    const at = reference.lastIndexOf("@");
    if (at < 0) {
      unpinned.push(reference);
      continue;
    }
    const pin = reference.slice(at + 1);
    if (!/^[0-9a-f]{40}$/.test(pin)) {
      unpinned.push(reference);
    }
  }
  if (unpinned.length > 0) {
    fail(`actions must be pinned to a 40-character commit sha: ${unpinned.join(", ")}`);
  }
  return usesLines.length;
}

// ---------------------------------------------------------------------------------------------
// CLI: run the checks CI can run from a checkout.
// ---------------------------------------------------------------------------------------------


/**
 * RC1-04: OpenSSL sources must be verified before extraction, on every path.
 *
 * Fails when a workflow extracts an openssl archive directly (`tar ... openssl`), or when it fetches
 * through the shared script without declaring the pin the script will verify against.
 */
function assertOpenSslVerified(workflowText, label = "workflow") {
  const lines = workflowText.split("\n");
  for (const line of lines) {
    const trimmed = line.trim();
    // A direct extraction of a downloaded OpenSSL archive bypasses the digest check.
    if (/^tar\s+-?[a-z]*x[a-z]*f?\s+.*openssl[^ ]*\.tar/.test(trimmed)) {
      fail(`${label}: extracts an OpenSSL archive directly instead of using scripts/fetch-openssl.sh`);
    }
    // Fetching through the shared script is only meaningful when the pin is *declared* (an interpolated
    // variable is not a declaration). A declared-but-empty pin still passes this wiring check and fails
    // closed inside the script at run time; that distinction is reported rather than hidden.
    if (trimmed.includes("scripts/fetch-openssl.sh")) {
      const declared = /^\s*OPENSSL_SHA256:\s*(\S.*)?$/m.exec(workflowText);
      if (!declared) {
        fail(`${label}: uses scripts/fetch-openssl.sh but never declares OPENSSL_SHA256`);
      }
      if (declared && !/^[0-9a-f]{64}$/.test((declared[1] || "").replace(/['"]/g, "").trim())) {
        console.log(`warn ${label}: OPENSSL_SHA256 is declared but not pinned; downloads will fail closed`);
      }
    }
  }
  return lines.length;
}


/**
 * RC1-03: the release DAG must not let packing happen without quality.
 *
 * Checks the structure rather than the prose: a `quality` job must exist and every job that produces or
 * signs artifacts must list it in `needs`. `skipped` is treated as not-success by GitHub once a job is in
 * `needs`, so this is the wiring half; the actual run evidence needs a runner.
 */
function assertReleaseDag(workflowText) {
  const jobs = new Map();
  let current = null;
  let inJobs = false;
  for (const line of workflowText.split("\n")) {
    if (/^jobs:\s*$/.test(line)) {
      inJobs = true;
      continue;
    }
    if (!inJobs) continue;
    const job = /^ {2}([a-z][a-z0-9_-]*):$/.exec(line);
    if (job) {
      current = job[1];
      jobs.set(current, { needs: [] });
      continue;
    }
    if (!current) continue;
    const needs = /^ {4}needs:\s*\[([^\]]*)\]/.exec(line);
    if (needs) {
      jobs.get(current).needs = needs[1].split(",").map((n) => n.trim()).filter(Boolean);
    }
  }
  if (!jobs.has("quality")) {
    fail("the release workflow has no quality job; packing would not be gated on fmt/clippy/tests");
  }
  const gated = ["pack", "sign", "publish", "release"].filter((j) => jobs.has(j));
  if (gated.length === 0) {
    fail("the release workflow has no pack/sign/publish job to gate");
  }
  for (const job of gated) {
    if (!jobs.get(job).needs.includes("quality")) {
      fail(`${job} does not depend on the quality job`);
    }
  }
  // A publish path must not exist implicitly: publishing is an owner decision, not a side effect of a tag.
  if (jobs.has("publish") && !/if:\s*.*(manual|dispatch|approval)/i.test(workflowText)) {
    fail("a publish job exists without an explicit manual/approval condition");
  }
  return [...jobs.keys()];
}

function main(argv) {
  const command = argv[2];
  const root = path.join(__dirname, "..");
  try {
    if (command === "actions") {
      const files = [".github/workflows/ci.yml", ".github/workflows/release.yml"];
      for (const file of files) {
        const text = fs.readFileSync(path.join(root, file), "utf8");
        const count = assertActionsPinned(text);
        console.log(`ok  ${file}: ${count} uses: entries are commit-pinned`);
      }
      return 0;
    }
    if (command === "openssl") {
      // RC1-04: every OpenSSL fetch must go through the shared verify-before-extract script, and the
      // pin must be declared in the workflow that uses it. A single unverified extract path is enough
      // to make the whole supply chain untrusted, so this checks all workflows, not one.
      const files = [".github/workflows/ci.yml", ".github/workflows/release.yml"];
      for (const file of files) {
        const text = fs.readFileSync(path.join(root, file), "utf8");
        assertOpenSslVerified(text, file);
      }
      console.log(`ok  every OpenSSL fetch in ${files.length} workflows is verified before extraction`);
      return 0;
    }
    if (command === "dag") {
      const text = fs.readFileSync(path.join(root, ".github/workflows/release.yml"), "utf8");
      const jobs = assertReleaseDag(text);
      console.log(`ok  release DAG gates artifacts on quality (jobs: ${jobs.join(", ")})`);
      return 0;
    }
    if (command === "lock") {
      const pinnedFile = path.join(root, "docs/release/Cargo.lock.sha256");
      if (!fs.existsSync(pinnedFile)) {
        console.error(`no pinned Cargo.lock digest at ${pinnedFile}`);
        return 2;
      }
      const pinned = fs.readFileSync(pinnedFile, "utf8").trim().split(/\s+/)[0];
      const actual = sha256File(path.join(root, "Cargo.lock"));
      assertDependencyLockUnchanged({ pinned, actual });
      console.log(`ok  Cargo.lock matches the pinned digest ${pinned}`);
      return 0;
    }
    console.error("usage: release-gate.js <actions|lock>");
    return 2;
  } catch (err) {
    console.error(`error: ${err.message}`);
    return 1;
  }
}

module.exports = {
  ReleaseGateError,
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
  assertReleaseDag
};

if (require.main === module) {
  process.exitCode = main(process.argv);
}
