#!/usr/bin/env node
"use strict";

/**
 * B-02: turn the bytes a release run actually produced into a verifiable attestation, and decide whether
 * the release may be called ready.
 *
 *   node scripts/release-attestation.js build --root <dir> --commit <sha> --sandbox <status> --out <file>
 *   node scripts/release-attestation.js verify <attestation.json> --root <dir>
 *   node scripts/release-attestation.js verdict <attestation.json> --provenance <doc> --root <dir>
 *
 * `verdict` prints ARTIFACT_SET_VERIFIED when the signature covers exactly the attested bytes from the
 * attested commit and the sandbox status is PASS; otherwise RELEASE_BLOCKED with the reasons. It never
 * claims the whole release is ready -- platform runtime evidence is not something it can see.
 *
 * The point of a separate attestation is that the signature must cover *these bytes*, produced by *this
 * commit*, on *this run*. A signature over a hand-picked file set proves the signing code works; it does not
 * prove the artifact set is the one being released.
 *
 * `build` is deterministic: no timestamps, artifacts sorted by name, canonical JSON. Two runs over the same
 * tree produce byte-identical output, which is what makes the package recomputable.
 */

const fs = require("node:fs");
const path = require("node:path");
const { sha256File, verifyDocument } = require("./provenance.js");
const { PRODUCTION_TRUSTED_KEYS } = require(path.join(__dirname, "..", "bin", "kroma.js"));

const SCHEMA = "kroma-release-attestation/1";

function fail(message) {
  throw new Error(message);
}

function walk(root) {
  const out = [];
  const stack = [""];
  while (stack.length) {
    const rel = stack.pop();
    const dir = path.join(root, rel);
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const childRel = rel ? path.join(rel, entry.name) : entry.name;
      if (entry.isDirectory()) {
        stack.push(childRel);
      } else if (entry.isFile()) {
        out.push(childRel);
      } else {
        fail(`refusing to attest a non-regular file: ${childRel}`);
      }
    }
  }
  return out.sort();
}

function describe(root, exclude) {
  return walk(root)
    .filter((rel) => rel !== exclude)
    .map((rel) => {
      const file = path.join(root, rel);
      return { name: rel.split(path.sep).join("/"), size: fs.statSync(file).size, sha256: sha256File(file) };
    });
}

function build({ root, commit, sandbox, out }) {
  if (!/^[0-9a-f]{40}$/.test(commit || "")) fail(`--commit must be a full 40-character sha, got ${commit}`);
  if (!["PASS", "HOST_CAPABILITY_BLOCKED", "FAIL"].includes(sandbox)) {
    fail(`--sandbox must be PASS, HOST_CAPABILITY_BLOCKED or FAIL, got ${sandbox}`);
  }
  const artifacts = describe(root, path.basename(out));
  if (artifacts.length === 0) fail(`no artifacts found under ${root}`);
  const doc = { schema: SCHEMA, git_commit: commit, sandbox_status: sandbox, artifacts };
  fs.writeFileSync(out, `${JSON.stringify(doc, null, 2)}\n`);
  console.log(`attested ${artifacts.length} artifact(s) from ${commit.slice(0, 12)} (sandbox: ${sandbox}) -> ${out}`);
}

function readAttestation(file) {
  const doc = JSON.parse(fs.readFileSync(file, "utf8"));
  if (doc.schema !== SCHEMA) fail(`unexpected attestation schema: ${doc.schema}`);
  if (!Array.isArray(doc.artifacts) || doc.artifacts.length === 0) fail("attestation lists no artifacts");
  return doc;
}

function verify({ document, root }) {
  const doc = readAttestation(document);
  const actual = describe(root, path.basename(document));
  const expected = new Map(doc.artifacts.map((a) => [a.name, a]));
  const problems = [];
  for (const artifact of actual) {
    const want = expected.get(artifact.name);
    if (!want) problems.push(`not covered by the attestation: ${artifact.name}`);
    else if (want.size !== artifact.size) problems.push(`size changed: ${artifact.name} (${want.size} -> ${artifact.size})`);
    else if (want.sha256 !== artifact.sha256) problems.push(`bytes changed: ${artifact.name}`);
    expected.delete(artifact.name);
  }
  for (const missing of expected.keys()) problems.push(`missing: ${missing}`);
  if (problems.length) fail(`attestation does not describe this tree:\n  ${problems.join("\n  ")}`);
  return doc;
}

function verdict({ document, provenance, root }) {
  const attestation = verify({ document, root });
  const reasons = [];
  let signedCommit = null;
  let signedNames = null;

  try {
    const signed = verifyDocument(fs.readFileSync(provenance, "utf8"), {
      trustedKeys: PRODUCTION_TRUSTED_KEYS,
      artifactRoot: root
    });
    signedCommit = signed.git_commit;
    signedNames = signed.artifacts.map((a) => a.name).sort();
  } catch (err) {
    reasons.push(`signature does not verify over these bytes: ${err.message}`);
  }

  if (signedCommit !== null && signedCommit !== attestation.git_commit) {
    reasons.push(`signature covers commit ${signedCommit.slice(0, 12)}, the artifacts were built from ${attestation.git_commit.slice(0, 12)}`);
  }
  if (signedNames !== null) {
    const attested = attestation.artifacts.map((a) => a.name).sort();
    const unsigned = attested.filter((n) => !signedNames.includes(n));
    const absent = signedNames.filter((n) => !attested.includes(n));
    if (unsigned.length) reasons.push(`attested but not signed: ${unsigned.join(", ")}`);
    if (absent.length) reasons.push(`signed but not attested: ${absent.join(", ")}`);
  }
  if (attestation.sandbox_status !== "PASS") {
    reasons.push(`sandbox isolation is ${attestation.sandbox_status}, not PASS`);
  }

  const ready = reasons.length === 0;
  const result = {
    schema: "kroma-release-verdict/1",
    git_commit: attestation.git_commit,
    // Deliberately not called RELEASE_READY: this verdict only says the signature covers exactly these
    // bytes from this commit and the sandbox really passed. Whether the *release* may proceed also depends
    // on platform runtime evidence, which no script here can decide.
    state: ready ? "ARTIFACT_SET_VERIFIED" : "RELEASE_BLOCKED",
    reasons
  };
  fs.writeFileSync("release-verdict.json", `${JSON.stringify(result, null, 2)}\n`);
  console.log(`state: ${result.state}`);
  for (const reason of reasons) console.log(`  - ${reason}`);
  return ready ? 0 : 1;
}

function parse(argv) {
  const args = { artifacts: [] };
  for (let i = 0; i < argv.length; i += 1) {
    const key = argv[i];
    if (key === "--root" || key === "--commit" || key === "--sandbox" || key === "--out" || key === "--provenance") {
      args[key.slice(2)] = argv[i + 1];
      i += 1;
    } else if (!key.startsWith("-")) {
      args.document = key;
    } else {
      fail(`unknown argument: ${key}`);
    }
  }
  return args;
}

function main(argv) {
  const command = argv[2];
  const args = parse(argv.slice(3));
  try {
    if (command === "build") {
      if (!args.root || !args.commit || !args.sandbox || !args.out) fail("build requires --root --commit --sandbox --out");
      build(args);
      return 0;
    }
    if (command === "verify") {
      if (!args.document || !args.root) fail("verify requires <attestation.json> --root <dir>");
      verify(args);
      console.log("attestation matches this tree");
      return 0;
    }
    if (command === "verdict") {
      if (!args.document || !args.provenance || !args.root) fail("verdict requires <attestation.json> --provenance <doc> --root <dir>");
      return verdict(args);
    }
    console.error("usage: release-attestation.js <build|verify|verdict> ...");
    return 2;
  } catch (err) {
    console.error(`error: ${err.message}`);
    return 1;
  }
}

process.exitCode = main(process.argv);
