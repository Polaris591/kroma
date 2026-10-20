"use strict";

/**
 * RC1-05: the release-provenance document.
 *
 * Two layers, deliberately separate:
 *  - the client-upgrade manifest (scripts/generate-release-manifest.js) is what an installed kroma
 *    verifies before replacing itself: one target, one tarball, one signature;
 *  - this provenance document is what a *release* is: every artifact with its length and sha256, the
 *    source commit, the gate results, and the keyId that signed it.
 *
 * They are not the same schema, and pretending otherwise is how "signed" ends up meaning nothing. This
 * module keeps the canonicalisation in one place so signing and verifying cannot drift.
 */

const crypto = require("node:crypto");
const fs = require("node:fs");
const path = require("node:path");

const SCHEMA = "kroma-release-provenance/1";
const MAX_DOC_BYTES = 256 * 1024;

class ProvenanceError extends Error {}

function fail(message) {
  throw new ProvenanceError(message);
}

/** Deterministic JSON: sorted keys, no whitespace. The signature is over exactly these bytes. */
function canonicalize(value) {
  if (Array.isArray(value)) {
    return `[${value.map(canonicalize).join(",")}]`;
  }
  if (value && typeof value === "object") {
    const keys = Object.keys(value).sort();
    return `{${keys.map((k) => `${JSON.stringify(k)}:${canonicalize(value[k])}`).join(",")}}`;
  }
  return JSON.stringify(value);
}

function validateArtifactName(name) {
  if (typeof name !== "string" || !/^[A-Za-z0-9._+-]+(?:\/[A-Za-z0-9._+-]+)*$/.test(name) ||
      name.split("/").some((part) => part === "." || part === "..")) fail(`invalid artifact name: ${name}`);
  return name;
}

function assertRegularFile(root, name) {
  validateArtifactName(name);
  let at = root;
  const pieces = name.split("/");
  for (let i = 0; i < pieces.length; i++) {
    at = path.join(at, pieces[i]);
    const st = fs.lstatSync(at);
    if (st.isSymbolicLink()) fail(`symlink in artifact path: ${name}`);
    if (i < pieces.length - 1 && !st.isDirectory()) fail(`non-directory in artifact path: ${name}`);
    if (i === pieces.length - 1 && !st.isFile()) fail(`not a regular artifact file: ${name}`);
  }
  return at;
}

function sha256File(file) {
  const hash = crypto.createHash("sha256");
  const fd = fs.openSync(file, "r");
  try {
    const buffer = Buffer.allocUnsafe(1 << 20);
    for (;;) {
      const read = fs.readSync(fd, buffer, 0, buffer.length, null);
      if (read === 0) break;
      hash.update(buffer.subarray(0, read));
    }
  } finally {
    fs.closeSync(fd);
  }
  return hash.digest("hex");
}

/** Build the unsigned document. Artifacts are measured from the files on disk, never from claims. */
function buildDocument({ version, gitCommit, artifacts, quality, issuedAt }) {
  if (!version || !/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(version)) {
    fail(`invalid version: ${version}`);
  }
  if (!gitCommit || !/^[0-9a-f]{40}$/.test(gitCommit)) {
    fail(`invalid git commit: ${gitCommit}`);
  }
  if (!Array.isArray(artifacts) || artifacts.length === 0) {
    fail("a provenance document must list at least one artifact");
  }
  const entries = artifacts.map(({ name, file }) => {
    validateArtifactName(name);
    if (!fs.existsSync(file)) fail(`artifact not found: ${file}`);
    const stat = fs.lstatSync(file);
    if (!stat.isFile()) fail(`artifact is not a regular file: ${file}`);
    return { name, length: stat.size, sha256: sha256File(file) };
  });
  const names = new Set(entries.map((e) => e.name));
  if (names.size !== entries.length) fail("artifact names must be unique");
  return {
    schema: SCHEMA,
    version,
    git_commit: gitCommit,
    issued_at: issuedAt || new Date().toISOString().replace(/\.\d+Z$/, "Z"),
    artifacts: entries,
    quality: quality || {},
    key_id: null,
    signature: null
  };
}

function signingPayload(document) {
  const { signature, ...rest } = document;
  return Buffer.from(canonicalize(rest), "utf8");
}

function signDocument(document, keyId, privateKey) {
  if (!/^[A-Za-z0-9_-]{1,64}$/.test(keyId || "")) fail(`invalid keyId: ${keyId}`);
  const signed = { ...document, key_id: keyId, signature: null };
  signed.signature = crypto.sign(null, signingPayload(signed), privateKey).toString("base64");
  return signed;
}

/**
 * Verify a document: schema, key, signature, and every artifact's length+sha256.
 *
 * `artifactRoot` is where the artifact files are expected to live; a document is only accepted if the
 * bytes it names are the bytes on disk.
 */
function verifyDocument(text, { trustedKeys, artifactRoot = ".", now = Date.now() } = {}) {
  if (typeof text !== "string") fail("document must be a string");
  if (Buffer.byteLength(text, "utf8") > MAX_DOC_BYTES) fail("document exceeds the size cap");
  let doc;
  try {
    doc = JSON.parse(text);
  } catch (err) {
    fail(`document is not valid JSON: ${err.message}`);
  }
  if (doc.schema !== SCHEMA) fail(`unsupported schema: ${doc.schema}`);
  if (!Array.isArray(doc.artifacts) || doc.artifacts.length === 0) fail("no artifacts listed");
  if (typeof doc.signature !== "string" || doc.signature.length !== 88) fail("invalid signature field");
  const keys = trustedKeys;
  if (!keys || typeof keys !== "object" || Object.keys(keys).length === 0) {
    fail("Production release trust root is unconfigured; provenance cannot be verified");
  }
  const rawKey = keys[doc.key_id];
  if (!rawKey) fail(`Unknown or untrusted signing key: ${doc.key_id}`);
  const { coercePublicKey } = require(path.join(__dirname, "..", "bin", "kroma.js"));
  const publicKey = coercePublicKey(rawKey);
  const ok = crypto.verify(
    null,
    signingPayload(doc),
    publicKey,
    Buffer.from(doc.signature, "base64")
  );
  if (!ok) fail("Signature verification failed for the provenance document");
  const names = new Set();
  for (const entry of doc.artifacts) {
    validateArtifactName(entry.name);
    if (names.has(entry.name)) fail(`duplicate signed artifact name: ${entry.name}`);
    names.add(entry.name);
    if (!Number.isSafeInteger(entry.length) || entry.length < 1 || !/^[0-9a-f]{64}$/.test(entry.sha256 || "")) {
      fail(`invalid signed artifact metadata: ${entry.name}`);
    }
    const file = path.join(artifactRoot, entry.name);
    if (!fs.existsSync(file)) fail(`artifact missing: ${entry.name}`);
    assertRegularFile(artifactRoot, entry.name);
    const actualLength = fs.statSync(file).size;
    if (actualLength !== entry.length) {
      fail(`${entry.name}: length ${actualLength} != ${entry.length}`);
    }
    const actualHash = sha256File(file);
    if (actualHash !== entry.sha256) fail(`${entry.name}: sha256 mismatch`);
  }
  return doc;
}

module.exports = {
  SCHEMA,
  ProvenanceError,
  canonicalize,
  sha256File,
  buildDocument,
  signDocument,
  verifyDocument,
  signingPayload,
  validateArtifactName
};
