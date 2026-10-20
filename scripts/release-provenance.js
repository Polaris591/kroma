#!/usr/bin/env node
"use strict";

/**
 * Owner-side release provenance signing. The owner signs ONLY the immutable candidate bytes from
 * one GitHub Actions run, including the attestation JSON which binds sandbox status and run id.
 * Never upload the private key; only the signed (public) JSON is sent to GitHub in pass 2.
 *
 *   node scripts/release-provenance.js sign-candidate \
 *     --candidate-root downloaded-bundle --version 1.5.0 \
 *     --key-id kroma-release-2026 --private-key-file /secure/ed25519.pem \
 *     --out /outside-bundle/signed-provenance.json
 *
 * The generic sign command remains for trusted maintenance of non-release provenance, but the
 * release-verdict CLI strictly requires the sign-candidate inventory and signed attestation.
 */
const fs = require("node:fs");
const path = require("node:path");
const crypto = require("node:crypto");
const { buildDocument, signDocument, verifyDocument } = require("./provenance.js");
const { verify: verifyAttestation } = require("./release-attestation.js");
const { exact } = require("./release-candidate.js");
const { PRODUCTION_TRUSTED_KEYS } = require(path.join(__dirname, "..", "bin", "kroma.js"));

function parseArgs(argv) {
  const args = { artifacts: [] };
  const values = new Set(["--version", "--git-commit", "--key-id", "--private-key-file", "--out",
    "--artifacts", "--candidate-root"]);
  for (let i = 0; i < argv.length; i += 1) {
    const key = argv[i], next = argv[i + 1];
    if (key === "--artifact") {
      if (!next || !next.includes("=")) throw new Error("--artifact expects name=path");
      const pos = next.indexOf("=");
      args.artifacts.push({ name: next.slice(0, pos), file: next.slice(pos + 1) });
      i++;
    } else if (values.has(key)) {
      if (!next || next.startsWith("--")) throw new Error(`missing value for ${key}`);
      args[key.slice(2).replace(/-([a-z])/g, (_, c) => c.toUpperCase())] = next;
      i++;
    } else if (key.startsWith("-")) throw new Error(`unknown argument: ${key}`);
    else if (!args.document) args.document = key;
    else throw new Error(`unexpected argument: ${key}`);
  }
  return args;
}
function readPrivateKey(file) {
  if (!file) throw new Error("signing requires --private-key-file");
  const stat = fs.lstatSync(file);
  if (!stat.isFile() || (stat.mode & 0o077) !== 0) {
    throw new Error("private key must be a non-symlink file unreadable by group/others");
  }
  const key = crypto.createPrivateKey(fs.readFileSync(file));
  if (key.asymmetricKeyType !== "ed25519") throw new Error("expected an Ed25519 private key");
  return key;
}
function signCandidate({ candidateRoot, version, keyId, privateKey, out }) {
  if (!out) throw new Error("sign-candidate requires --out outside bundle");
  if (path.resolve(out).startsWith(path.resolve(candidateRoot) + path.sep)) {
    throw new Error("signed provenance must live outside the candidate bundle");
  }
  exact(fs.readdirSync(candidateRoot), ["candidate", "release-attestation.json"], "owner candidate bundle");
  const attestationFile = path.join(candidateRoot, "release-attestation.json");
  const attestation = verifyAttestation({ document: attestationFile, root: path.join(candidateRoot, "candidate") });
  if (attestation.sandbox_status !== "PASS") throw new Error(`cannot sign sandbox status ${attestation.sandbox_status}`);
  if (attestation.version !== version) throw new Error("requested version differs from signed candidate version");
  const artifacts = [
    ...attestation.artifacts.map((a) => ({ name: `candidate/${a.name}`, file: path.join(candidateRoot, "candidate", a.name) })),
    { name: "release-attestation.json", file: attestationFile }
  ];
  const doc = buildDocument({ version, gitCommit: attestation.git_commit, artifacts,
    quality: { source_repository: attestation.source_repository, source_run_id: attestation.source_run_id,
      source_run_attempt: attestation.source_run_attempt, sandbox_status: attestation.sandbox_status } });
  const signed = signDocument(doc, keyId, privateKey);
  fs.writeFileSync(out, JSON.stringify(signed, null, 2) + "\n", { mode: 0o644, flag: "wx" });
  console.log(`signed ${signed.artifacts.length} immutable candidate artifact(s) from run ${attestation.source_run_id}`);
  return signed;
}
function main(argv) {
  try {
    const cmd = argv[2], a = parseArgs(argv.slice(3));
    if (cmd === "sign-candidate") {
      if (!a.candidateRoot || !a.version || !a.keyId) throw new Error("sign-candidate requires --candidate-root --version --key-id");
      signCandidate({ candidateRoot: a.candidateRoot, version: a.version, keyId: a.keyId,
        privateKey: readPrivateKey(a.privateKeyFile), out: a.out });
      return 0;
    }
    if (cmd === "sign") {
      const doc = buildDocument({ version: a.version, gitCommit: a.gitCommit, artifacts: a.artifacts });
      const signed = signDocument(doc, a.keyId, readPrivateKey(a.privateKeyFile));
      const out = a.out || "release-provenance.json";
      fs.writeFileSync(out, `${JSON.stringify(signed, null, 2)}\n`, { flag: "wx", mode: 0o644 });
      console.log(`signed ${signed.artifacts.length} artifact(s) -> ${out}`);
      return 0;
    }
    if (cmd === "verify") {
      if (!a.document) throw new Error("verify requires a document path");
      const doc = verifyDocument(fs.readFileSync(a.document, "utf8"), {
        trustedKeys: PRODUCTION_TRUSTED_KEYS, artifactRoot: a.artifactRoot || path.dirname(a.document) });
      console.log(`verified ${doc.artifacts.length} artifact(s) against ${doc.key_id}`);
      return 0;
    }
    throw new Error("usage: release-provenance.js <sign-candidate|sign|verify> ...");
  } catch (err) { console.error(`error: ${err.message}`); return 1; }
}
if (require.main === module) process.exitCode = main(process.argv);
module.exports = { signCandidate, main };
