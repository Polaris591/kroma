#!/usr/bin/env node
"use strict";

/**
 * Release candidate protocol v2.
 *
 * A generated attestation is NOT a signed claim. Its status and run identity acquire authority
 * only when the owner's signed provenance includes the exact release-attestation.json bytes, and
 * the provenance signature and the complete candidate bundle are verified together.
 *
 *   build --root candidate --version V --commit SHA --repo OWNER/REPO --run-id ID
 *         --run-attempt N --sandbox STATUS --out release-attestation.json
 *   verify release-attestation.json --root candidate
 *   verdict release-attestation.json --provenance signed.json --root bundle
 *           --expected-repo OWNER/REPO --expected-run-id ID --expected-run-attempt N
 *           --expected-commit SHA --out verdict.json
 */
const fs = require("node:fs");
const path = require("node:path");
const { sha256File, verifyDocument } = require("./provenance.js");
const { checkCandidate, expectedCandidate, assertVersion, exact } = require("./release-candidate.js");
const { PRODUCTION_TRUSTED_KEYS } = require(path.join(__dirname, "..", "bin", "kroma.js"));

const SCHEMA = "kroma-release-attestation/2";
const WORKFLOW = ".github/workflows/release.yml";
const MAX_BYTES = 256 * 1024;
function fail(message) { throw new Error(message); }
function isCommit(value) { return typeof value === "string" && /^[0-9a-f]{40}$/.test(value); }
function isRunId(value) { return typeof value === "string" && /^[1-9][0-9]{0,18}$/.test(value); }
function isRepo(value) { return typeof value === "string" && /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(value); }
function validateIdentity(doc) {
  if (!isCommit(doc.git_commit)) fail("attestation git_commit must be a full SHA");
  if (!isRepo(doc.source_repository)) fail("invalid attestation source_repository");
  if (!isRunId(doc.source_run_id)) fail("invalid attestation source_run_id");
  if (!Number.isSafeInteger(doc.source_run_attempt) || doc.source_run_attempt < 1) fail("invalid source_run_attempt");
  if (doc.workflow !== WORKFLOW) fail("unsupported release workflow identity");
  if (!["PASS", "HOST_CAPABILITY_BLOCKED", "FAIL"].includes(doc.sandbox_status)) fail("unknown sandbox status");
  assertVersion(doc.version);
}
function describe(root) {
  const output = [];
  for (const name of fs.readdirSync(root).sort()) {
    const file = path.join(root, name);
    const stat = fs.lstatSync(file);
    if (!stat.isFile() || stat.size <= 0) fail(`not a nonempty regular candidate file: ${name}`);
    output.push({ name, size: stat.size, sha256: sha256File(file) });
  }
  return output;
}
function sandboxEvidence(root) {
  const file = path.join(root, "sandbox-evidence.json");
  if (fs.lstatSync(file).isSymbolicLink()) fail("sandbox evidence cannot be a symlink");
  const text = fs.readFileSync(file, "utf8");
  if (Buffer.byteLength(text) > 64 * 1024) fail("sandbox evidence too large");
  let evidence;
  try { evidence = JSON.parse(text); } catch (_) { fail("invalid sandbox evidence JSON"); }
  if (!evidence || evidence.schema !== "kroma-sandbox-evidence/1" ||
      evidence.job !== "sandbox isolation (real bwrap)" ||
      evidence.test_script !== "scripts/run-sandbox-tests.sh") fail("invalid sandbox evidence identity");
  const expectedStatus = evidence.exit_code === 0 ? "PASS" : evidence.exit_code === 2 ? "HOST_CAPABILITY_BLOCKED" :
    evidence.exit_code === 1 ? "FAIL" : null;
  if (!expectedStatus || evidence.status !== expectedStatus) fail("sandbox evidence exit code contradicts its status");
  return evidence;
}
function validateEvidence(doc, root) {
  const evidence = sandboxEvidence(root);
  for (const [source, match] of [
    ["git_commit", "git_commit"], ["source_repository", "source_repository"],
    ["source_run_id", "source_run_id"], ["source_run_attempt", "source_run_attempt"]
  ]) if (evidence[source] !== doc[match]) fail(`sandbox evidence ${source} does not match attestation`);
  if (evidence.status !== doc.sandbox_status) fail("sandbox status does not match the sandbox evidence file");
}
function build({ root, out, commit, repo, runId, runAttempt, version, sandbox }) {
  const doc = {
    schema: SCHEMA, version, git_commit: commit, source_repository: repo,
    source_run_id: String(runId || ""), source_run_attempt: Number(runAttempt), workflow: WORKFLOW,
    sandbox_status: sandbox, artifacts: []
  };
  validateIdentity(doc);
  if (path.resolve(out).startsWith(path.resolve(root) + path.sep)) fail("attestation must live outside candidate/");
  checkCandidate(root, doc.version);
  validateEvidence(doc, root);
  doc.artifacts = describe(root);
  fs.writeFileSync(out, `${JSON.stringify(doc, null, 2)}\n`, { flag: "wx", mode: 0o644 });
  return doc;
}
function readAttestation(file) {
  const bytes = fs.readFileSync(file);
  if (!bytes.length || bytes.length > MAX_BYTES) fail("invalid attestation size");
  let doc;
  try { doc = JSON.parse(bytes.toString("utf8")); } catch (err) { fail(`invalid attestation JSON: ${err.message}`); }
  const keys = ["schema", "version", "git_commit", "source_repository", "source_run_id",
    "source_run_attempt", "workflow", "sandbox_status", "artifacts"];
  if (!doc || doc.schema !== SCHEMA || typeof doc !== "object") fail("unexpected attestation schema");
  exact(Object.keys(doc), keys, "attestation fields");
  // Disallow JSON duplicate keys, alternate encodings and unrecognized fields: the owner signs one
  // specific serialization with one interpretation.
  if (bytes.toString("utf8") !== `${JSON.stringify(doc, null, 2)}\n`) fail("attestation must use the canonical v2 serialization");
  validateIdentity(doc);
  if (!Array.isArray(doc.artifacts)) fail("attestation artifacts must be an array");
  const names = doc.artifacts.map((a) => a.name);
  exact(names, expectedCandidate(doc.version), "attested release");
  if (new Set(names).size !== names.length) fail("duplicate attestation artifact");
  for (const a of doc.artifacts) {
    if (!Number.isSafeInteger(a.size) || a.size < 1 || !/^[0-9a-f]{64}$/.test(a.sha256 || "")) fail(`invalid attestation size/digest: ${a.name}`);
    exact(Object.keys(a), ["name", "size", "sha256"], "artifact fields");
  }
  return doc;
}
function verify({ document, root }) {
  const doc = readAttestation(document);
  checkCandidate(root, doc.version);
  validateEvidence(doc, root);
  const actual = describe(root);
  for (let i = 0; i < actual.length; i++) {
    if (actual[i].name !== doc.artifacts[i].name) fail("attestation entries not sorted");
    if (actual[i].size !== doc.artifacts[i].size) fail(`size changed: ${actual[i].name}`);
    if (actual[i].sha256 !== doc.artifacts[i].sha256) fail(`bytes changed: ${actual[i].name}`);
  }
  return doc;
}
function verdict({ document, provenance, root, expectedRepo, expectedRunId, expectedRunAttempt,
  expectedCommit, trustedKeys = PRODUCTION_TRUSTED_KEYS, out }) {
  if (!out) fail("verdict requires --out outside the candidate bundle");
  if (path.resolve(out).startsWith(path.resolve(root) + path.sep)) fail("verdict output must live outside the signed bundle");
  const attestation = verify({ document, root: path.join(root, "candidate") });
  const reasons = [];
  let signed = null;
  try {
    signed = verifyDocument(fs.readFileSync(provenance, "utf8"), { trustedKeys, artifactRoot: root });
  } catch (err) { reasons.push(`invalid signature or artifact: ${err.message}`); }
  if (signed) {
    if (signed.git_commit !== attestation.git_commit) reasons.push("signed commit does not match attestation commit");
    if (signed.version !== attestation.version) reasons.push("signed version does not match attestation version");
    for (const [key, want] of Object.entries({
      source_repository: attestation.source_repository, source_run_id: attestation.source_run_id,
      source_run_attempt: attestation.source_run_attempt, sandbox_status: attestation.sandbox_status
    })) {
      if (signed.quality?.[key] !== want) reasons.push(`signed quality claim ${key} disagrees with attestation`);
    }
    const required = ["release-attestation.json", ...attestation.artifacts.map((a) => `candidate/${a.name}`)];
    try { exact(signed.artifacts.map((a) => a.name), required, "signed artifact"); }
    catch (err) { reasons.push(err.message); }
  }
  if (path.resolve(document) !== path.resolve(root, "release-attestation.json")) reasons.push("attestation not at the canonical bundle path");
  try {
    exact(fs.readdirSync(root), ["candidate", "release-attestation.json"], "signed bundle directory");
  } catch (err) { reasons.push(err.message); }
  if (attestation.sandbox_status !== "PASS") reasons.push(`sandbox isolation did not pass: ${attestation.sandbox_status}`);
  if (!expectedRepo || !expectedRunId || !expectedRunAttempt || !expectedCommit) {
    reasons.push("verdict requires an independently verified source repo, run, attempt and commit");
  } else {
    if (attestation.source_repository !== expectedRepo) reasons.push("source repository mismatches verified GitHub run");
    if (attestation.source_run_id !== String(expectedRunId)) reasons.push("source run id mismatches verified GitHub run");
    if (attestation.source_run_attempt !== Number(expectedRunAttempt)) reasons.push("source run attempt mismatches verified GitHub run");
    if (attestation.git_commit !== expectedCommit) reasons.push("source commit mismatches verified GitHub run");
  }
  const result = {
    schema: "kroma-release-verdict/2", git_commit: attestation.git_commit,
    source_repository: attestation.source_repository, source_run_id: attestation.source_run_id,
    state: reasons.length ? "RELEASE_BLOCKED" : "ARTIFACT_SET_VERIFIED", reasons
  };
  fs.writeFileSync(out, JSON.stringify(result, null, 2) + "\n", { flag: "w", mode: 0o644 });
  console.log(`state: ${result.state}`);
  for (const reason of reasons) console.log(`  - ${reason}`);
  return reasons.length ? 1 : 0;
}
function parse(argv) {
  const a = {};
  const allowed = ["root", "out", "commit", "repo", "run-id", "run-attempt", "version", "sandbox",
    "provenance", "expected-repo", "expected-run-id", "expected-run-attempt", "expected-commit"];
  for (let i = 0; i < argv.length; i++) {
    const key = argv[i];
    if (key.startsWith("--")) {
      if (!allowed.includes(key.slice(2)) || i + 1 >= argv.length) fail(`unknown or incomplete option: ${key}`);
      a[key.slice(2).replace(/-([a-z])/g, (_, c) => c.toUpperCase())] = argv[++i];
    } else if (!a.document) a.document = key;
    else fail(`extra argument: ${key}`);
  }
  return a;
}
function main(argv) {
  try {
    const command = argv[2], a = parse(argv.slice(3));
    if (command === "build") {
      if (!a.root || !a.out || !a.commit || !a.repo || !a.runId || !a.runAttempt || !a.version || !a.sandbox) fail("build requires root/out/source/run/version/sandbox");
      build(a);
      console.log(`attested candidate from ${a.commit.slice(0, 12)} run ${a.runId}`);
      return 0;
    }
    if (command === "verify") {
      if (!a.document || !a.root) fail("verify needs document and --root candidate");
      verify(a); console.log("candidate bytes match attestation (UNSIGNED)"); return 0;
    }
    if (command === "verdict") {
      if (!a.document || !a.provenance || !a.root || !a.out) fail("verdict needs attestation, --provenance, --root bundle, --out");
      return verdict(a);
    }
    fail("usage: release-attestation.js <build|verify|verdict> ...");
  } catch (err) { console.error(`error: ${err.message}`); return 1; }
}
if (require.main === module) process.exitCode = main(process.argv);
module.exports = { SCHEMA, build, verify, verdict, readAttestation, sandboxEvidence, validateEvidence, main };
