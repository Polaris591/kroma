#!/usr/bin/env node
"use strict";

/**
 * RC1-05 tests: the provenance document must not be forgeable and must not be satisfiable by a
 * document whose artifacts do not match the bytes on disk.
 *
 * The tests use an ephemeral key pair and an injected trust root; the production key is exercised
 * separately (scripts/release-provenance.js sign/verify) because the private key must never be needed
 * by a test run.
 */

const assert = require("node:assert");
const crypto = require("node:crypto");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const {
  buildDocument,
  signDocument,
  verifyDocument,
  ProvenanceError
} = require("./provenance.js");
const { PRODUCTION_TRUSTED_KEYS } = require(path.join(__dirname, "..", "bin", "kroma.js"));

const COMMIT = "a".repeat(40);
let checks = 0;
let failures = 0;

function ok(name, fn) {
  checks += 1;
  try {
    fn();
    console.log(`  ok  ${name}`);
  } catch (err) {
    failures += 1;
    console.log(`  FAIL ${name}\n       ${err.message}`);
  }
}

function refuses(fn, pattern) {
  try {
    fn();
  } catch (err) {
    if (!(err instanceof ProvenanceError) && !(err instanceof Error)) throw err;
    assert.match(err.message, pattern);
    return;
  }
  throw new Error(`expected a refusal matching ${pattern}`);
}

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "kroma-provenance-"));
const artifact = path.join(dir, "kroma-linux-x64");
fs.writeFileSync(artifact, "binary-bytes");

const { privateKey, publicKey } = crypto.generateKeyPairSync("ed25519");
const rawHex = publicKey.export({ format: "der", type: "spki" }).subarray(12).toString("hex");
const trusted = { "kroma-release-2026": rawHex };

function freshDoc() {
  return buildDocument({
    version: "1.5.0",
    gitCommit: COMMIT,
    artifacts: [{ name: "kroma-linux-x64", file: artifact }],
    issuedAt: "2026-10-18T00:00:00Z"
  });
}

console.log("RC1-05: release provenance");
ok("a signed document verifies against the trust root and the artifacts on disk", () => {
  const signed = signDocument(freshDoc(), "kroma-release-2026", privateKey);
  const doc = verifyDocument(JSON.stringify(signed), { trustedKeys: trusted, artifactRoot: dir });
  assert.strictEqual(doc.artifacts.length, 1);
  assert.strictEqual(doc.artifacts[0].sha256, crypto.createHash("sha256").update("binary-bytes").digest("hex"));
  assert.strictEqual(doc.key_id, "kroma-release-2026");
});

ok("a tampered artifact is refused", () => {
  const signed = signDocument(freshDoc(), "kroma-release-2026", privateKey);
  // Same length on purpose, so this exercises the digest check rather than the size check.
  fs.writeFileSync(artifact, "tamper-bytes");
  refuses(
    () => verifyDocument(JSON.stringify(signed), { trustedKeys: trusted, artifactRoot: dir }),
    /sha256 mismatch/
  );
  fs.writeFileSync(artifact, "binary-bytes");
});

ok("an artifact whose size changed is refused", () => {
  const signed = signDocument(freshDoc(), "kroma-release-2026", privateKey);
  fs.writeFileSync(artifact, "binary-bytes-plus");
  refuses(
    () => verifyDocument(JSON.stringify(signed), { trustedKeys: trusted, artifactRoot: dir }),
    /length \d+ != \d+/
  );
  fs.writeFileSync(artifact, "binary-bytes");
});

ok("a manifest claiming a key that is not trusted is refused", () => {
  const signed = signDocument(freshDoc(), "kroma-release-2026", privateKey);
  refuses(
    () => verifyDocument(JSON.stringify(signed), { trustedKeys: { other: rawHex }, artifactRoot: dir }),
    /Unknown or untrusted signing key/
  );
});

ok("a signature from a different key is refused even when the keyId is trusted", () => {
  const signed = signDocument(freshDoc(), "kroma-release-2026", privateKey);
  const { privateKey: other } = crypto.generateKeyPairSync("ed25519");
  const forged = { ...signed, signature: crypto.sign(null, Buffer.from("nope"), other).toString("base64") };
  refuses(
    () => verifyDocument(JSON.stringify(forged), { trustedKeys: trusted, artifactRoot: dir }),
    /Signature verification failed/
  );
});

ok("editing the document after signing is refused", () => {
  const signed = signDocument(freshDoc(), "kroma-release-2026", privateKey);
  const edited = { ...signed, version: "9.9.9" };
  refuses(
    () => verifyDocument(JSON.stringify(edited), { trustedKeys: trusted, artifactRoot: dir }),
    /Signature verification failed/
  );
});

ok("an unknown schema (an older or newer document) is refused", () => {
  const signed = signDocument(freshDoc(), "kroma-release-2026", privateKey);
  const old = { ...signed, schema: "kroma-release-provenance/0" };
  refuses(
    () => verifyDocument(JSON.stringify(old), { trustedKeys: trusted, artifactRoot: dir }),
    /unsupported schema/
  );
});

ok("an unconfigured trust root refuses before touching any artifact", () => {
  const signed = signDocument(freshDoc(), "kroma-release-2026", privateKey);
  refuses(
    () => verifyDocument(JSON.stringify(signed), { trustedKeys: {}, artifactRoot: dir }),
    /unconfigured/
  );
});

ok("the production trust root holds exactly the release key", () => {
  assert.deepStrictEqual(Object.keys(PRODUCTION_TRUSTED_KEYS), ["kroma-release-2026"]);
  assert.match(PRODUCTION_TRUSTED_KEYS["kroma-release-2026"], /^[0-9a-f]{64}$/);
});

fs.rmSync(dir, { recursive: true, force: true });
console.log("");
if (failures > 0) {
  console.error(`provenance suite FAILED after ${checks - failures} passing check(s)`);
  process.exitCode = 1;
} else {
  console.log(`all ${checks} checks passed`);
}
