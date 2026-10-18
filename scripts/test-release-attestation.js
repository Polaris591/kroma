#!/usr/bin/env node
"use strict";

/**
 * Negative tests for scripts/release-attestation.js.
 *
 * The attestation is only worth something if it refuses the cases that matter: changed bytes, a file that
 * appeared after signing, a file that vanished, a wrong commit, and a sandbox that did not actually pass.
 * The RELEASE_READY path needs the production key, so it is exercised once for real during sealing rather
 * than faked here with a test key.
 */

const assert = require("node:assert");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { execFileSync } = require("node:child_process");

const SCRIPT = path.join(__dirname, "release-attestation.js");
const COMMIT = "0123456789abcdef0123456789abcdef01234567";

let passed = 0;
function check(name, fn) {
  fn();
  passed += 1;
  console.log(`ok ${passed} - ${name}`);
}

function run(args, expectFailure) {
  try {
    const out = execFileSync(process.execPath, [SCRIPT, ...args], { encoding: "utf8", stdio: "pipe" });
    if (expectFailure) throw new Error(`expected failure, got success: ${out}`);
    return out;
  } catch (err) {
    if (!expectFailure) throw new Error(`${args[0]} should have succeeded: ${err.stderr || err.message}`);
    // The verdict prints its reasons on stdout and signals failure with the exit code, so both streams
    // matter when asserting on the message.
    return `${err.stdout || ""}${err.stderr || ""}` || err.message;
  }
}

function fixture() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-attest-"));
  fs.writeFileSync(path.join(root, "kroma-linux-x64"), "binary-one");
  fs.writeFileSync(path.join(root, "kroma-linux-arm64-musl"), "binary-two");
  return root;
}

// Attestation documents live outside the tree they describe: a document written inside the tree would
// itself become an artifact on the next build, which is how two "identical" builds stop being identical.
function docPath(root, name) {
  return path.join(fs.mkdtempSync(path.join(os.tmpdir(), "kroma-attest-doc-")), name || "att.json");
}

check("build is deterministic", () => {
  const root = fixture();
  const a = docPath(root, "a.json");
  const b = docPath(root, "b.json");
  run(["build", "--root", root, "--commit", COMMIT, "--sandbox", "PASS", "--out", a]);
  run(["build", "--root", root, "--commit", COMMIT, "--sandbox", "PASS", "--out", b]);
  assert.strictEqual(fs.readFileSync(a, "utf8"), fs.readFileSync(b, "utf8"));
});

check("build refuses a short commit", () => {
  const root = fixture();
  run(["build", "--root", root, "--commit", "deadbeef", "--sandbox", "PASS", "--out", docPath(root, "x.json")], true);
});

check("build refuses an unknown sandbox status", () => {
  const root = fixture();
  run(["build", "--root", root, "--commit", COMMIT, "--sandbox", "PROBABLY", "--out", docPath(root, "x.json")], true);
});

check("verify accepts the tree it described", () => {
  const root = fixture();
  const doc = docPath(root);
  run(["build", "--root", root, "--commit", COMMIT, "--sandbox", "PASS", "--out", doc]);
  run(["verify", doc, "--root", root]);
});

check("verify rejects changed bytes", () => {
  const root = fixture();
  const doc = docPath(root);
  run(["build", "--root", root, "--commit", COMMIT, "--sandbox", "PASS", "--out", doc]);
  fs.writeFileSync(path.join(root, "kroma-linux-x64"), "binary-ONE");
  const err = run(["verify", doc, "--root", root], true);
  assert.match(err, /bytes changed: kroma-linux-x64/);
});

check("verify rejects a file that appeared after attestation", () => {
  const root = fixture();
  const doc = docPath(root);
  run(["build", "--root", root, "--commit", COMMIT, "--sandbox", "PASS", "--out", doc]);
  fs.writeFileSync(path.join(root, "kroma-extra"), "surprise");
  const err = run(["verify", doc, "--root", root], true);
  assert.match(err, /not covered by the attestation: kroma-extra/);
});

check("verify rejects a missing file", () => {
  const root = fixture();
  const doc = docPath(root);
  run(["build", "--root", root, "--commit", COMMIT, "--sandbox", "PASS", "--out", doc]);
  fs.unlinkSync(path.join(root, "kroma-linux-arm64-musl"));
  const err = run(["verify", doc, "--root", root], true);
  assert.match(err, /missing: kroma-linux-arm64-musl/);
});

check("verdict is RELEASE_BLOCKED when the sandbox did not pass", () => {
  const root = fixture();
  const doc = docPath(root);
  run(["build", "--root", root, "--commit", COMMIT, "--sandbox", "HOST_CAPABILITY_BLOCKED", "--out", doc]);
  const err = run(["verdict", doc, "--provenance", docPath(root, "nope.json"), "--root", root], true);
  assert.match(err, /sandbox isolation is HOST_CAPABILITY_BLOCKED/);
  assert.match(err, /signature does not verify/);
});

check("verdict is RELEASE_BLOCKED without a signature document", () => {
  const root = fixture();
  const doc = docPath(root);
  run(["build", "--root", root, "--commit", COMMIT, "--sandbox", "PASS", "--out", doc]);
  const err = run(["verdict", doc, "--provenance", docPath(root, "absent.json"), "--root", root], true);
  assert.match(err, /RELEASE_BLOCKED|state: RELEASE_BLOCKED/);
});

console.log(`\nall ${passed} checks passed`);
